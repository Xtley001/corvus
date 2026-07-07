// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IAaveV3Pool {
    // Aave flash loan — PULLS repayment via approve+transferFrom after executeOperation returns.
    // Fee: 0.09% of amount. Must approve(AAVE_POOL, amount + premium) before returning.
    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16 referralCode) external;
    function liquidationCall(address collateralAsset, address debtAsset, address user, uint256 debtToCover, bool receiveAToken) external;
    function supply(address asset, uint256 amount, address onBehalfOf, uint16 referralCode) external;
    function borrow(address asset, uint256 amount, uint256 interestRateMode, uint16 referralCode, address onBehalfOf) external;
    function repay(address asset, uint256 amount, uint256 rateMode, address onBehalfOf) external returns (uint256);
    function withdraw(address asset, uint256 amount, address to) external returns (uint256);
    function getUserAccountData(address user) external view returns (
        uint256 totalCollateralBase, uint256 totalDebtBase, uint256 availableBorrowsBase,
        uint256 currentLiquidationThreshold, uint256 ltv, uint256 healthFactor
    );
    function getReserveData(address asset) external view returns (
        uint256 configuration, uint128 liquidityIndex, uint128 currentLiquidityRate,
        uint128 variableBorrowIndex, uint128 currentVariableBorrowRate, uint128 currentStableBorrowRate,
        uint40 lastUpdateTimestamp, uint16 id, address aTokenAddress, address stableDebtTokenAddress,
        address variableDebtTokenAddress, address interestRateStrategyAddress,
        uint128 accruedToTreasury, uint128 unbacked, uint128 isolationModeTotalDebt
    );
}

interface IAaveV3PoolAddressesProvider {
    // Aave V3 Pool address is fetched from this provider at deploy time — never hardcoded
    function getPool() external view returns (address);
}
