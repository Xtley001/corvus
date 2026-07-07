// SPDX-License-Identifier: MIT
pragma solidity ^0.8.24;
import "forge-std/Script.sol";
import "../src/FlashExecutor.sol";

/// @notice Deploys FlashExecutor to Base Mainnet.
///
/// Required environment variables:
///   DEPLOYER_PRIVATE_KEY    — private key of the deploying wallet (uint256). This key
///                             holds NO privileges on the deployed contract — it only pays gas.
///   OWNER_ADDRESS           — AUDIT 4.1: the contract owner (whitelists, sweep-to-any,
///                             executor rotation). SET THIS TO A GNOSIS SAFE / HARDWARE WALLET.
///   EXECUTOR_ADDRESS        — hot wallet EOA that submits flash loan txs
///   COLD_WALLET_ADDRESS     — sweep destination; hardcoded into contract at deploy
///   BASESCAN_API_KEY        — for source verification
contract Deploy is Script {
    function run() external {
        address owner      = vm.envAddress("OWNER_ADDRESS");
        address executor   = vm.envAddress("EXECUTOR_ADDRESS");
        address coldWallet = vm.envAddress("COLD_WALLET_ADDRESS");

        require(owner      != address(0), "OWNER_ADDRESS not set");
        require(executor   != address(0), "EXECUTOR_ADDRESS not set");
        require(coldWallet != address(0), "COLD_WALLET_ADDRESS not set");

        vm.startBroadcast(vm.envUint("DEPLOYER_PRIVATE_KEY"));
        // AUDIT 4.1: owner is explicit — deployer EOA gets NO rights on the contract.
        FlashExecutor f = new FlashExecutor(owner, executor, coldWallet);
        vm.stopBroadcast();

        console.log("FlashExecutor:          ", address(f));
        console.log("Owner:                  ", owner);
        console.log("Executor:               ", executor);
        console.log("Aave V3 Pool (resolved):", f.AAVE_V3_POOL());
        console.log("Cold Wallet:            ", coldWallet);
    }
}
