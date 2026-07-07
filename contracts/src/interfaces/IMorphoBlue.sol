// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IMorphoBlue {
    struct MarketParams {
        address loanToken;
        address collateralToken;
        address oracle;
        address irm;
        uint256 lltv;
    }

    struct Market {
        uint128 totalSupplyAssets;
        uint128 totalSupplyShares;
        uint128 totalBorrowAssets;
        uint128 totalBorrowShares;
        uint128 lastUpdate;
        uint128 fee;
    }

    // Morpho flash loan — PULLS repayment via safeTransferFrom after callback returns.
    // Caller MUST: IERC20(token).approve(MORPHO_BLUE, assets) inside onMorphoFlashLoan.
    // Do NOT push tokens to Morpho — it will pull them.
    function flashLoan(address token, uint256 assets, bytes calldata data) external;

    function liquidate(
        MarketParams calldata marketParams,
        address borrower,
        uint256 seizedAssets,
        uint256 repaidShares,
        bytes calldata data
    ) external returns (uint256 seizedAssets_, uint256 repaidAssets_);

    function supply(MarketParams calldata, uint256 assets, uint256 shares, address onBehalf, bytes calldata) external returns (uint256, uint256);
    function borrow(MarketParams calldata, uint256 assets, uint256 shares, address onBehalf, address receiver) external returns (uint256, uint256);
    function repay(MarketParams calldata, uint256 assets, uint256 shares, address onBehalf, bytes calldata) external returns (uint256, uint256);
    function withdraw(MarketParams calldata, uint256 assets, uint256 shares, address onBehalf, address receiver) external returns (uint256, uint256);
    function market(bytes32 id) external view returns (Market memory);
}

interface IMorphoFlashLoanCallback {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}
