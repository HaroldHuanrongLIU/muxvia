import { dirname, resolve } from "node:path"

import {
  controlPlaneRelease,
  dispatchDiagnostic,
  parseInvocation,
  routingServiceRelease,
} from "./diagnostic-cli"
import {
  embeddedBundleIdentity,
  ReleaseBundleError,
  validateReleaseBundle,
} from "./release-bundle"
import { checkForUpdate } from "./update-notification"

const bundleIdentity = embeddedBundleIdentity()
let bundledServicePath: string | undefined
if (bundleIdentity) {
  try {
    const bundle = await validateReleaseBundle(process.execPath, bundleIdentity)
    bundledServicePath = bundle.routingServicePath
  } catch (error) {
    const code = error instanceof ReleaseBundleError ? error.code : "release-bundle-invalid"
    process.stderr.write(`${code}: release bundle verification failed\n`)
    process.exit(78)
  }
}

const invocation = parseInvocation(
  Bun.argv.slice(2),
  process.env,
  bundleIdentity ? process.execPath : undefined,
)
if (bundledServicePath && resolve(invocation.servicePath) !== bundledServicePath) {
  process.stderr.write("release-bundle-invalid: routing service must come from the verified bundle\n")
  process.exit(78)
}
if (await dispatchDiagnostic(invocation)) process.exit(process.exitCode ?? 0)

const update = bundleIdentity
  ? await checkForUpdate({
      currentRelease: bundleIdentity.release,
      muxviaHome: dirname(dirname(invocation.socketPath)),
    }).catch(() => undefined)
  : undefined

const { run } = await import("./app")
await run({
  servicePath: invocation.servicePath,
  socketPath: invocation.socketPath,
  release: controlPlaneRelease,
  serviceRelease: routingServiceRelease,
  updateRelease: update?.release,
})
