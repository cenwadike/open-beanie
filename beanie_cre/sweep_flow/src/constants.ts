import { keccak256, parseAbiParameters, toBytes } from "viem";

export const MERCHANT_REGISTERED_SIG = keccak256(toBytes("MerchantRegistered(address,address)"));
export const RECEIVER_ANNOUNCED_SIG = keccak256(toBytes("ReceiverAnnounced(address,address,uint256)"));
export const WEBHOOK_URL_SET_SIG = keccak256(toBytes("WebhookUrlSet(address,string)"));
export const TRANSFER_SIG = keccak256(toBytes("Transfer(address,address,uint256)"));

export const SWEEP_SELECTOR = keccak256(toBytes("sweep()")).slice(0, 10) as `0x${string}`;
export const REGISTER_MERCHANT_SELECTOR = keccak256(
  toBytes("registerMerchant(address,bytes32,bytes32)"),
).slice(0, 10) as `0x${string}`;

export const CALL3_PARAMS = parseAbiParameters("(address target, bool allowFailure, bytes callData)[]");
export const REGISTER_MERCHANT_PARAMS = parseAbiParameters("address, bytes32, bytes32");
