import { Config } from "./config";

export type HttpRequester = {
  sendRequest: (req: unknown) => { result: () => { body: Uint8Array; statusCode: number } };
};

export type Log = {
  address: string;
  topics: string[];
  data: string;
  blockNumber: string;
  transactionHash: string;
};

export function rpcCall<T>(
  requester: HttpRequester,
  rpcUrl: string,
  method: string,
  params: unknown[],
): T {
  const body = JSON.stringify({ jsonrpc: "2.0", id: 1, method, params });
  const response = requester
    .sendRequest({
      url: rpcUrl,
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: new TextEncoder().encode(body),
    })
    .result();

  if (response.statusCode < 200 || response.statusCode >= 300) {
    throw new Error(`RPC ${method} failed: HTTP ${response.statusCode}`);
  }

  const parsed = JSON.parse(new TextDecoder().decode(response.body));
  if (parsed.error) throw new Error(`RPC ${method} error: ${JSON.stringify(parsed.error)}`);
  return parsed.result as T;
}

export function rpcBatchCall<T>(
  requester: HttpRequester,
  rpcUrl: string,
  calls: { method: string; params: unknown[] }[],
): T[] {
  if (calls.length === 0) return [];

  const body = JSON.stringify(
    calls.map((c, i) => ({ jsonrpc: "2.0", id: i, method: c.method, params: c.params })),
  );

  const response = requester
    .sendRequest({
      url: rpcUrl,
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: new TextEncoder().encode(body),
    })
    .result();

  if (response.statusCode < 200 || response.statusCode >= 300) {
    throw new Error(`RPC batch failed: HTTP ${response.statusCode}`);
  }

  const parsed = JSON.parse(new TextDecoder().decode(response.body)) as Array<{
    id: number;
    result?: T;
    error?: unknown;
  }>;

  // Batch responses aren't guaranteed to come back in request order per the
  // JSON-RPC 2.0 spec — sort by id before mapping back to caller order.
  const byId = new Map(parsed.map((r) => [r.id, r]));
  return calls.map((_, i) => {
    const entry = byId.get(i);
    if (!entry) throw new Error(`RPC batch: missing response for id ${i}`);
    if (entry.error) throw new Error(`RPC batch error [id ${i}]: ${JSON.stringify(entry.error)}`);
    return entry.result as T;
  });
}

/**
 * Fetches logs in fixed-size block chunks, enforced with a strict maximum HTTP call limit per tick.
 */
export function getLogsChunked(
  requester: HttpRequester,
  rpcUrl: string,
  address: string,
  topics: (string | string[] | null)[],
  fromBlock: number,
  toBlock: number,
  chunkSize: number,
  maxCalls: number = 3,
): Log[] {
  const logs: Log[] = [];

  // Prevent endless loops if fromBlock is 0 or far behind the chain tip:
  // Calculate the lowest start block allowed based on (chunkSize * maxCalls) backwards from toBlock.
  const minAllowedStart = Math.max(fromBlock, toBlock - chunkSize * maxCalls + 1);
  let start = minAllowedStart;
  let callsMade = 0;

  while (start <= toBlock && callsMade < maxCalls) {
    const end = Math.min(start + chunkSize - 1, toBlock);
    logs.push(
      ...rpcCall<Log[]>(requester, rpcUrl, "eth_getLogs", [
        {
          address,
          topics,
          fromBlock: `0x${start.toString(16)}`,
          toBlock: `0x${end.toString(16)}`,
        },
      ]),
    );
    start = end + 1;
    callsMade++;
  }

  return logs;
}