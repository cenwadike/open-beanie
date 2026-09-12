import { z } from "zod";

export const configSchema = z.object({
  chainSelectorName: z.string(),
  isTestnet: z.boolean().default(false),
  rpcUrl: z.string(),
  factoryAddress: z.string(),
  webhookRegistryAddress: z.string(),
  tokenAddress: z.string(),
  creKeeperReceiverAddress: z.string(),
  registryStartBlock: z.number(),
  webhookRegistryStartBlock: z.number(),
  logChunkBlocks: z.number().default(2000),
  depositScanBlocks: z.number().default(500),
  schedule: z.string().default("*/3 * * * * *"),
});

export type Config = z.infer<typeof configSchema>;


