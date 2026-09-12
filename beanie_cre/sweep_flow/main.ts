import { Runner } from "@chainlink/cre-sdk";
import { configSchema, initWorkflow, type Config } from "./src/workflow";

export async function main() {
  const runner = await Runner.newRunner<Config>({ configSchema: configSchema as never })
  await runner.run(initWorkflow)
}

main();
