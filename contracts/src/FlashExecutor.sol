// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

import "./interfaces/IBalancerVault.sol";
import "./interfaces/IMorphoBlue.sol";
import "./interfaces/IAaveV3.sol";
import "./interfaces/IAerodrome.sol";
import "./interfaces/IUniswapV3.sol";
import "@openzeppelin/contracts/token/ERC20/IERC20.sol";
import "@openzeppelin/contracts/token/ERC20/utils/SafeERC20.sol";

/// @title  FlashExecutor v1.1 — Corvus atomic flash-loan execution engine
/// @notice Base Mainnet (Chain ID: 8453)
/// @dev    v1.1 changes:
///           - Removed StrategyType.BACKRUN (S6), BackrunParams, and _executeBackrun.
///           - _executeTriArb: added allowedRouters whitelist check (was missing; executor
///             key compromise previously allowed arbitrary calls through tri-arb path).
///           - _handleCallback: added profitToken != address(0) guard.
contract FlashExecutor {
    using SafeERC20 for IERC20;

    // ─── Reentrancy guard (uint256 saves ~19,900 gas vs bool) ────────────
    uint256 private constant _NOT_ENTERED = 1;
    uint256 private constant _ENTERED     = 2;
    uint256 private _status               = _NOT_ENTERED;

    modifier nonReentrant() {
        require(_status != _ENTERED, "Corvus: reentrant call");
        _status = _ENTERED;
        _;
        _status = _NOT_ENTERED;
    }

    // FIX (SHOWSTOPPER): _inFlashLoan replaces nonReentrant on flash callbacks.
    // execute() holds _status = _ENTERED while the flash provider calls back.
    // Putting nonReentrant on the callbacks too caused every flash loan to revert
    // ("Corvus: reentrant call") because the guard fired on its own callback.
    // _inFlashLoan is set to true only by execute() and checked by the callbacks
    // so they can only be entered via the expected flash-loan path.
    bool private _inFlashLoan;
    modifier onlyFlashLoan() {
        require(_inFlashLoan, "Corvus: not in flash loan");
        _;
    }

    // ─── Emergency pause ──────────────────────────────────────────────────
    bool public paused;
    modifier whenNotPaused() { require(!paused, "Corvus: paused"); _; }
    function setPaused(bool _paused) external onlyOwner { paused = _paused; emit Paused(_paused); }

    // ─── Protocol addresses (Base Mainnet) ────────────────────────────────
    address public constant BALANCER_VAULT    = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    address public constant MORPHO_BLUE       = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address public constant AERODROME_ROUTER  = 0xcF77a3Ba9A5CA399B7c97c74d54e5b1Beb874E43;
    address public constant AERODROME_FACTORY  = 0x420DD381b31aEf6683db6B902084cB0FFECe40Da;
    address public constant UNISWAP_V3_ROUTER = 0x2626664c2603336E57B271c5C0b26F421741e481;
    address public constant UNI_POS_MANAGER   = 0x03a520b32C04BF3bEEf7BEb72E919cf822Ed34f1;
    address public immutable AAVE_V3_POOL;

    // ─── Access control ───────────────────────────────────────────────────
    address public immutable owner;
    address public executor;
    address public immutable coldWallet;

    // F-07 FIX: Two-step executor change with 24-hour timelock.
    // Instant setExecutor() allows an attacker to atomically hijack the executor
    // and drain all tokens on owner key compromise — no grace period to react.
    address public pendingExecutor;
    uint256 public executorChangeETA;

    // ─── Protocol + router whitelists ─────────────────────────────────────
    mapping(address => bool) public allowedProtocols;
    mapping(address => bool) public allowedRouters;

    // ─── Events ───────────────────────────────────────────────────────────
    event Executed(StrategyType indexed stratType, uint256 profit);
    event ProtocolWhitelisted(address indexed proto, bool allowed);
    event RouterWhitelisted(address indexed router, bool allowed);
    event Paused(bool indexed isPaused);
    // F-07: Timelock events
    event ExecutorProposed(address indexed proposed, uint256 eta);
    event ExecutorAccepted(address indexed newExecutor);

    enum StrategyType {
        CROSS_DEX_ARB,   // 0
        TRI_ARB,         // 1
        LIQUIDATION,     // 2
        FLASH_JIT,       // 3
        RATE_ARB_OPEN,   // 4
        RATE_ARB_CLOSE   // 5
    }

    enum FlashProvider { BALANCER, MORPHO, AAVE }

    struct ExecuteParams {
        FlashProvider provider;
        address[]     tokens;
        uint256[]     amounts;
        StrategyType  stratType;
        bytes         stratData;
        uint256       minProfit;
        address       profitToken;
    }

    modifier onlyExecutor() { require(msg.sender == executor, "Corvus: not executor"); _; }
    modifier onlyOwner()    { require(msg.sender == owner,    "Corvus: not owner");    _; }

    constructor(address _owner, address _executor, address _coldWallet) {
        // F-08 FIX: Chain ID guard — prevents silent wrong-chain deployment.
        // Hardcoded protocol addresses (Aave, Balancer, Morpho) are Base-specific.
        // A wrong-chain deploy would point at whatever happens to live at those
        // addresses on another chain, silently corrupting all protocol interactions.
        require(block.chainid == 8453, "Corvus: wrong chain — expected Base Mainnet (8453)");
        // AUDIT 4.1 FIX: owner is now an explicit constructor parameter, NOT msg.sender.
        // This lets you deploy from a throwaway deployer EOA while setting owner to a
        // Gnosis Safe / hardware wallet — the intended cold-storage owner from block 0,
        // without an ownership-transfer step (owner remains immutable).
        require(_owner      != address(0), "Corvus: zero owner");
        require(_executor   != address(0), "Corvus: zero executor");
        require(_coldWallet != address(0), "Corvus: zero cold wallet");
        owner      = _owner;
        executor   = _executor;
        coldWallet = _coldWallet;
        AAVE_V3_POOL = IAaveV3PoolAddressesProvider(
            0xe20fCBdBfFC4Dd138cE8b2E6FBb6CB49777ad64D
        ).getPool();

        allowedRouters[UNISWAP_V3_ROUTER] = true;
        allowedRouters[AERODROME_ROUTER]  = true;
        allowedProtocols[AAVE_V3_POOL]    = true;
        allowedProtocols[MORPHO_BLUE]     = true;
    }

    // F-07 FIX: Two-step executor change with 24-hour timelock.
    // Step 1: owner proposes a new executor — starts the 24h delay.
    function proposeExecutor(address _new) external onlyOwner {
        require(_new != address(0), "Corvus: zero address");
        pendingExecutor    = _new;
        executorChangeETA  = block.timestamp + 24 hours;
        emit ExecutorProposed(_new, executorChangeETA);
    }

    // Step 2: owner accepts after the delay has elapsed.
    function acceptExecutor() external onlyOwner {
        require(pendingExecutor != address(0), "Corvus: no pending executor");
        require(block.timestamp >= executorChangeETA, "Corvus: timelock not elapsed");
        executor        = pendingExecutor;
        pendingExecutor = address(0);
        executorChangeETA = 0;
        emit ExecutorAccepted(executor);
    }

    function setAllowedProtocol(address proto, bool allowed) external onlyOwner {
        allowedProtocols[proto] = allowed;
        emit ProtocolWhitelisted(proto, allowed);
    }

    function setAllowedRouter(address router, bool allowed) external onlyOwner {
        allowedRouters[router] = allowed;
        emit RouterWhitelisted(router, allowed);
    }

    // ─── Entry Point ──────────────────────────────────────────────────────
    function execute(ExecuteParams calldata p)
        external nonReentrant onlyExecutor whenNotPaused
    {
        address flashToken = p.tokens.length > 0 ? p.tokens[0] : address(0);
        bytes memory cbData = abi.encode(
            p.stratType, p.stratData, p.minProfit, p.profitToken, flashToken
        );

        // FIX: set _inFlashLoan before calling the provider so callbacks pass onlyFlashLoan.
        _inFlashLoan = true;

        if (p.provider == FlashProvider.BALANCER) {
            IBalancerVault(BALANCER_VAULT).flashLoan(address(this), p.tokens, p.amounts, cbData);
        } else if (p.provider == FlashProvider.MORPHO) {
            require(p.tokens.length == 1, "Morpho: single asset");
            IMorphoBlue(MORPHO_BLUE).flashLoan(p.tokens[0], p.amounts[0], cbData);
        } else {
            require(p.tokens.length == 1, "Aave: single asset");
            IAaveV3Pool(AAVE_V3_POOL).flashLoanSimple(address(this), p.tokens[0], p.amounts[0], cbData, 0);
        }

        _inFlashLoan = false;
    }

    // ─── Flash loan callbacks ─────────────────────────────────────────────
    // FIX: callbacks now use onlyFlashLoan instead of nonReentrant.
    // nonReentrant on these caused every flash loan to revert because execute()
    // already held _status = _ENTERED when the provider called back.

    function receiveFlashLoan(
        IERC20[] calldata tokens, uint256[] calldata amounts,
        uint256[] calldata feeAmounts, bytes calldata userData
    ) external onlyFlashLoan {
        require(msg.sender == BALANCER_VAULT, "Corvus: unauthorized");
        _handleCallback(userData);
        for (uint256 i = 0; i < tokens.length; i++) {
            tokens[i].safeTransfer(BALANCER_VAULT, amounts[i] + feeAmounts[i]);
        }
    }

    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external onlyFlashLoan {
        require(msg.sender == MORPHO_BLUE, "Corvus: unauthorized");
        (,,,, address flashToken) = abi.decode(data, (StrategyType, bytes, uint256, address, address));
        _handleCallback(data);
        _forceApprove(flashToken, MORPHO_BLUE, assets);
    }

    function executeOperation(
        address asset, uint256 amount, uint256 premium,
        address initiator, bytes calldata params
    ) external onlyFlashLoan returns (bool) {
        require(msg.sender == AAVE_V3_POOL,  "Corvus: unauthorized");
        require(initiator == address(this),  "Corvus: invalid initiator");
        _handleCallback(params);
        _forceApprove(asset, AAVE_V3_POOL, amount + premium);
        return true;
    }

    // ─── Dispatcher ───────────────────────────────────────────────────────
    function _handleCallback(bytes calldata data) internal {
        (StrategyType stratType, bytes memory stratData, uint256 minProfit, address profitToken,)
            = abi.decode(data, (StrategyType, bytes, uint256, address, address));

        require(profitToken != address(0), "Corvus: zero profit token");
        uint256 startBalance = IERC20(profitToken).balanceOf(address(this));

        if      (stratType == StrategyType.CROSS_DEX_ARB)  _executeCrossDexArb(stratData);
        else if (stratType == StrategyType.TRI_ARB)        _executeTriArb(stratData);
        else if (stratType == StrategyType.LIQUIDATION)    _executeLiquidation(stratData);
        else if (stratType == StrategyType.FLASH_JIT)      _executeFlashJIT(stratData);
        else if (stratType == StrategyType.RATE_ARB_OPEN)  _executeRateArbOpen(stratData);
        else if (stratType == StrategyType.RATE_ARB_CLOSE) _executeRateArbClose(stratData);
        else revert("Corvus: unknown strategy");

        uint256 profit = IERC20(profitToken).balanceOf(address(this)) - startBalance;
        require(profit >= minProfit, "Corvus: insufficient profit");
        emit Executed(stratType, profit);
    }

    function _forceApprove(address token, address spender, uint256 amount) internal {
        IERC20(token).safeApprove(spender, 0);
        if (amount > 0) IERC20(token).safeApprove(spender, amount);
    }

    // ─── Structured swap step (AUDIT 2.1/2.2 FIX) ─────────────────────────
    // Swap calldata is now built ON-CHAIN from the live token balance instead of
    // being passed as opaque off-chain bytes. This eliminates two whole bug classes:
    //   • amountIn = type(uint256).max in off-chain calldata (SwapRouter02 takes it
    //     literally → transferFrom reverts). On-chain we use the real balance.
    //   • wrong recipient in off-chain calldata (output sent to router, not executor).
    //     On-chain we always set recipient = address(this).
    struct SwapStep {
        address router;    // must be whitelisted
        address tokenIn;
        address tokenOut;
        uint24  fee;       // UniV3 fee (ppm); ignored for Aerodrome
        bool    stable;    // Aerodrome stable-pool flag; ignored for UniV3
        bool    isUniV3;   // true = UniV3 exactInputSingle, false = Aerodrome
        uint256 minOut;    // per-leg slippage floor
    }

    /// Execute one swap leg using `amountIn` of tokenIn taken from this contract's
    /// live balance. Builds router calldata on-chain. Returns tokenOut received.
    function _executeSwapStep(SwapStep memory s, uint256 amountIn) internal returns (uint256) {
        require(allowedRouters[s.router], "Corvus: router not whitelisted");
        require(amountIn > 0, "Corvus: zero swap input");

        _forceApprove(s.tokenIn, s.router, amountIn);
        uint256 balBefore = IERC20(s.tokenOut).balanceOf(address(this));

        bool ok;
        if (s.isUniV3) {
            // exactInputSingle((tokenIn,tokenOut,fee,recipient,amountIn,amountOutMin,sqrtPriceLimitX96))
            IUniswapV3Router.ExactInputSingleParams memory ep = IUniswapV3Router.ExactInputSingleParams({
                tokenIn: s.tokenIn, tokenOut: s.tokenOut, fee: s.fee, recipient: address(this),
                amountIn: amountIn, amountOutMinimum: s.minOut, sqrtPriceLimitX96: 0
            });
            (ok,) = s.router.call(abi.encodeCall(IUniswapV3Router.exactInputSingle, (ep)));
        } else {
            // Aerodrome: swapExactTokensForTokens(amountIn, amountOutMin, Route[], to, deadline)
            // Route = (from, to, stable, factory)
            IAerodromeRouter.Route[] memory routes = new IAerodromeRouter.Route[](1);
            routes[0] = IAerodromeRouter.Route({
                from: s.tokenIn, to: s.tokenOut, stable: s.stable, factory: AERODROME_FACTORY
            });
            bytes memory cd = abi.encodeWithSelector(
                IAerodromeRouter.swapExactTokensForTokens.selector,
                amountIn, s.minOut, routes, address(this), block.timestamp
            );
            (ok,) = s.router.call(cd);
        }
        require(ok, "Corvus: swap failed");

        uint256 received = IERC20(s.tokenOut).balanceOf(address(this)) - balBefore;
        require(received >= s.minOut, "Corvus: swap slippage");
        // AUDIT 4.2 FIX: reset router allowance to 0 after the call — no standing
        // approvals left on a contract that accumulates profit between trades.
        _forceApprove(s.tokenIn, s.router, 0);
        return received;
    }

    // ─── Cross-DEX Arb ────────────────────────────────────────────────────
    struct CrossDexArbParams {
        uint256  amountIn;
        SwapStep buy;
        SwapStep sell;
    }
    function _executeCrossDexArb(bytes memory data) internal {
        CrossDexArbParams memory p = abi.decode(data, (CrossDexArbParams));
        uint256 bought = _executeSwapStep(p.buy, p.amountIn);
        _executeSwapStep(p.sell, bought);
        // Overall profit is enforced by the _handleCallback profit gate.
    }

    // ─── Tri-Arb ──────────────────────────────────────────────────────────
    struct TriArbParams {
        uint256     amountIn;
        SwapStep[3] steps;
    }
    function _executeTriArb(bytes memory data) internal {
        TriArbParams memory p = abi.decode(data, (TriArbParams));
        uint256 amt = p.amountIn;
        for (uint8 i = 0; i < 3; i++) {
            amt = _executeSwapStep(p.steps[i], amt);
        }
    }

    // ─── Liquidation ──────────────────────────────────────────────────────
    // AUDIT 2.2 FIX: the collateral→debt swap is now built ON-CHAIN via exactInput
    // using the actual seized collateral amount and recipient = address(this).
    // Previously the off-chain calldata used amountIn = MAX (revert) and recipient
    // = router (profit lost). `swapPath` is the packed UniV3 path (single or multi-hop).
    struct LiquidationParams {
        uint8 protocol;
        address borrower; address collateralAsset; address debtAsset;
        uint256 debtToCover;
        bytes morphoMarketData;
        address swapRouter; bytes swapPath;
    }
    function _executeLiquidation(bytes memory data) internal {
        LiquidationParams memory p = abi.decode(data, (LiquidationParams));
        require(allowedRouters[p.swapRouter], "Corvus: swap router not whitelisted");

        uint256 collBefore = IERC20(p.collateralAsset).balanceOf(address(this));
        if (p.protocol == 0) {
            IMorphoBlue.MarketParams memory mp = abi.decode(p.morphoMarketData, (IMorphoBlue.MarketParams));
            _forceApprove(p.debtAsset, MORPHO_BLUE, p.debtToCover);
            IMorphoBlue(MORPHO_BLUE).liquidate(mp, p.borrower, 0, type(uint256).max, "");
        } else {
            _forceApprove(p.debtAsset, AAVE_V3_POOL, p.debtToCover);
            IAaveV3Pool(AAVE_V3_POOL).liquidationCall(
                p.collateralAsset, p.debtAsset, p.borrower, p.debtToCover, false
            );
        }
        uint256 collReceived = IERC20(p.collateralAsset).balanceOf(address(this)) - collBefore;
        require(collReceived > 0, "Corvus: no collateral — already liquidated");

        _forceApprove(p.collateralAsset, p.swapRouter, collReceived);
        // exactInput with the live collateral amount; overall profit enforced by the gate.
        IUniswapV3Router(p.swapRouter).exactInput(
            IUniswapV3Router.ExactInputParams({
                path: p.swapPath, recipient: address(this),
                amountIn: collReceived, amountOutMinimum: 0
            })
        );
        // AUDIT 4.2: never leave a standing router allowance on a contract that holds funds.
        _forceApprove(p.collateralAsset, p.swapRouter, 0);
    }

    // ─── Flash JIT ────────────────────────────────────────────────────────
    struct FlashJITParams {
        address token0; address token1; uint24 fee;
        int24 tickLower; int24 tickUpper;
        uint256 amount0; uint256 amount1;
        uint256 amount0Min; uint256 amount1Min;
        bool zeroForOne; uint256 swapAmountIn;
        address swapRouter; bytes swapCalldata;
    }
    function _executeFlashJIT(bytes memory data) internal {
        FlashJITParams memory p = abi.decode(data, (FlashJITParams));
        if (p.amount0 > 0) _forceApprove(p.token0, UNI_POS_MANAGER, p.amount0);
        if (p.amount1 > 0) _forceApprove(p.token1, UNI_POS_MANAGER, p.amount1);

        (uint256 tokenId, uint128 liquidity,,) = INonfungiblePositionManager(UNI_POS_MANAGER).mint(
            INonfungiblePositionManager.MintParams({
                token0: p.token0, token1: p.token1, fee: p.fee,
                tickLower: p.tickLower, tickUpper: p.tickUpper,
                amount0Desired: p.amount0, amount1Desired: p.amount1,
                amount0Min: p.amount0Min, amount1Min: p.amount1Min,
                recipient: address(this), deadline: block.timestamp
            })
        );

        address swapIn = p.zeroForOne ? p.token0 : p.token1;
        _forceApprove(swapIn, p.swapRouter, p.swapAmountIn);
        (bool ok,) = p.swapRouter.call(p.swapCalldata);
        require(ok, "Corvus: JIT swap failed");
        _forceApprove(swapIn, p.swapRouter, 0); // AUDIT 4.2: reset allowance

        INonfungiblePositionManager(UNI_POS_MANAGER).decreaseLiquidity(
            INonfungiblePositionManager.DecreaseLiquidityParams({
                tokenId: tokenId, liquidity: liquidity,
                amount0Min: 0, amount1Min: 0, deadline: block.timestamp
            })
        );
        INonfungiblePositionManager(UNI_POS_MANAGER).collect(
            INonfungiblePositionManager.CollectParams({
                tokenId: tokenId, recipient: address(this),
                amount0Max: type(uint128).max, amount1Max: type(uint128).max
            })
        );
        INonfungiblePositionManager(UNI_POS_MANAGER).burn(tokenId);
    }

    // ─── Rate Arb Open/Close ──────────────────────────────────────────────
    struct RateArbOpenParams  {
        address supplyProto; address borrowProto;
        uint8 loops; bytes[] supplyDatas; bytes[] borrowDatas;
    }
    struct RateArbCloseParams {
        address supplyProto; address borrowProto;
        bytes[] repayDatas; bytes[] withdrawDatas;
    }
    function _executeRateArbOpen(bytes memory data) internal {
        RateArbOpenParams memory p = abi.decode(data, (RateArbOpenParams));
        require(allowedProtocols[p.supplyProto], "Corvus: supply proto not whitelisted");
        require(allowedProtocols[p.borrowProto], "Corvus: borrow proto not whitelisted");
        for (uint8 i = 0; i < p.loops; i++) {
            (bool ok,) = p.supplyProto.call(p.supplyDatas[i]); require(ok, "Corvus: supply failed");
            (ok,)      = p.borrowProto.call(p.borrowDatas[i]); require(ok, "Corvus: borrow failed");
        }
    }
    function _executeRateArbClose(bytes memory data) internal {
        RateArbCloseParams memory p = abi.decode(data, (RateArbCloseParams));
        require(allowedProtocols[p.supplyProto], "Corvus: supply proto not whitelisted");
        require(allowedProtocols[p.borrowProto], "Corvus: borrow proto not whitelisted");
        for (uint256 i = 0; i < p.repayDatas.length; i++) {
            (bool ok,) = p.borrowProto.call(p.repayDatas[i]); require(ok, "Corvus: repay failed");
        }
        for (uint256 i = 0; i < p.withdrawDatas.length; i++) {
            (bool ok,) = p.supplyProto.call(p.withdrawDatas[i]); require(ok, "Corvus: withdraw failed");
        }
    }

    // ─── Admin ────────────────────────────────────────────────────────────
    function sweep(address token, address to) external onlyOwner {
        uint256 bal = IERC20(token).balanceOf(address(this));
        require(bal > 0, "Corvus: nothing to sweep");
        IERC20(token).safeTransfer(to, bal);
    }

    function emergencySweep(address token) external onlyExecutor {
        uint256 bal = IERC20(token).balanceOf(address(this));
        if (bal > 0) IERC20(token).safeTransfer(coldWallet, bal);
    }

    function sweepETH(address payable to) external onlyOwner {
        require(address(this).balance > 0, "Corvus: no ETH");
        (bool ok,) = to.call{value: address(this).balance}("");
        require(ok, "Corvus: ETH sweep failed");
    }

    receive() external payable {}
}
