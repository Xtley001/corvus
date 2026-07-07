// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IAerodromeRouter {
    struct Route { address from; address to; bool stable; address factory; }
    function swapExactTokensForTokens(uint256 amountIn, uint256 amountOutMin, Route[] calldata routes, address to, uint256 deadline) external returns (uint256[] memory);
    function getAmountsOut(uint256 amountIn, Route[] calldata routes) external view returns (uint256[] memory);
}

interface IAerodromePool {
    function getReserves() external view returns (uint256 reserve0, uint256 reserve1, uint256 blockTimestampLast);
    function token0() external view returns (address);
    function token1() external view returns (address);
    function stable() external view returns (bool);
    function fee() external view returns (uint256);
    function swap(uint256 amount0Out, uint256 amount1Out, address to, bytes calldata data) external;
}

interface IAerodromeFactory {
    // Pool addresses resolved at runtime from this factory
    function getPool(address tokenA, address tokenB, bool stable) external view returns (address pool);
    function getFee(address pool, bool stable) external view returns (uint256);
}
