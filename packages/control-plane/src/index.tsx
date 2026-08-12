import { isAbsolute } from "node:path"

import { run } from "./app"

function valueAfter(flag: string): string {
  const index = Bun.argv.indexOf(flag)
  const value = index >= 0 ? Bun.argv[index + 1] : undefined
  if (!value) throw new Error(`Missing ${flag}`)
  return value
}

const servicePath = valueAfter("--service")
const socketPath = valueAfter("--socket")
if (!isAbsolute(servicePath) || !isAbsolute(socketPath)) {
  throw new Error("--service and --socket must be absolute paths")
}

await run({
  servicePath,
  socketPath,
  release: "muxvia-dev",
})
