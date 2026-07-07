// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;

interface IBalancerVault {
    // Balancer V2 flash loan — feeAmounts[] is always [0,0,...]. Fee is always zero.
    // Repayment: caller pushes tokens back via safeTransfer(BALANCER_VAULT, amount)
    function flashLoan(
        address recipient,
        address[] calldata tokens,
        uint256[] calldata amounts,
        bytes calldata userData
    ) external;

    function swap(
        SingleSwap calldata singleSwap,
        FundManagement calldata funds,
        uint256 limit,
        uint256 deadline
    ) external payable returns (uint256);

    struct SingleSwap {
        bytes32 poolId;
        SwapKind kind;
        address assetIn;
        address assetOut;
        uint256 amount;
        bytes userData;
    }

    struct FundManagement {
        address sender;
        bool fromInternalBalance;
        address payable recipient;
        bool toInternalBalance;
    }

    enum SwapKind { GIVEN_IN, GIVEN_OUT }
}
