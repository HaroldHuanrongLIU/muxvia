#!/usr/bin/env bun

import solidPlugin from "@opentui/solid/bun-plugin"
import { resolve } from "node:path"

const args = Bun.argv.slice(2)

function option(name: string): string {
  const index = args.indexOf(name)
  const value = index >= 0 ? args[index + 1] : undefined
  if (!value || value.startsWith("--")) throw new Error(`missing ${name}`)
  return value
}

const target = option("--target")
if (![
  "bun-darwin-arm64",
  "bun-darwin-x64",
  "bun-linux-arm64",
  "bun-linux-x64-baseline",
].includes(target)) throw new Error("unsupported --target")

const result = await Bun.build({
  entrypoints: [resolve("packages/control-plane/src/release-index.ts")],
  target: "bun",
  tsconfig: resolve("packages/control-plane/tsconfig.json"),
  plugins: [solidPlugin],
  define: {
    MUXVIA_BUNDLE_RELEASE: JSON.stringify(option("--release")),
    MUXVIA_ROUTING_RELEASE: JSON.stringify(option("--routing-release")),
    MUXVIA_BUNDLE_TARGET: JSON.stringify(option("--bundle-target")),
    MUXVIA_BUNDLE_BUILD: JSON.stringify(option("--build")),
  },
  compile: {
    target: target as Bun.Build.CompileTarget,
    outfile: resolve(option("--output")),
    autoloadBunfig: false,
    autoloadDotenv: false,
  },
})

if (!result.success) throw new AggregateError(result.logs, "Control Plane release build failed")
