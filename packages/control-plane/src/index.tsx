import {
  controlPlaneRelease,
  dispatchDiagnostic,
  parseInvocation,
  routingServiceRelease,
} from "./diagnostic-cli"

const invocation = parseInvocation(Bun.argv.slice(2))
if (await dispatchDiagnostic(invocation)) process.exit(process.exitCode ?? 0)

const { run } = await import("./app")
await run({
  servicePath: invocation.servicePath,
  socketPath: invocation.socketPath,
  release: controlPlaneRelease,
  serviceRelease: routingServiceRelease,
})
