import { afterEach, expect, test } from "bun:test"
import { mkdtemp, readFile, rm } from "node:fs/promises"
import { tmpdir } from "node:os"
import { join } from "node:path"

import {
  PUBLIC_RELEASE_MANIFEST_URL,
  checkForUpdate,
} from "../src/update-notification"

const roots: string[] = []

afterEach(async () => {
  await Promise.all(roots.splice(0).map((root) => rm(root, { recursive: true, force: true })))
})

async function home(): Promise<string> {
  const root = await mkdtemp(join(tmpdir(), "muxvia-update-"))
  roots.push(root)
  return root
}

test("checks one fixed public manifest with a bodyless GET and reports only a newer release", async () => {
  const root = await home()
  const requests: Array<{ input: string; init?: RequestInit }> = []
  const notice = await checkForUpdate({
    currentRelease: "0.1.0",
    muxviaHome: root,
    now: () => 1_000_000,
    fetch: async (input, init) => {
      requests.push({ input: String(input), init })
      return Response.json({ schemaVersion: 1, product: "muxvia", release: "0.2.0" })
    },
  })

  expect(notice).toEqual({ release: "0.2.0" })
  expect(requests).toEqual([{ input: PUBLIC_RELEASE_MANIFEST_URL, init: {
    method: "GET",
    headers: { accept: "application/json" },
    redirect: "follow",
    signal: expect.any(AbortSignal),
  } }])
  expect(await readFile(join(root, "state/update-check.json"), "utf8")).not.toContain("0.1.0")
})

test("persists the attempt before I/O and checks at most once per 24 hours even after failure", async () => {
  const root = await home()
  let calls = 0
  const options = {
    currentRelease: "0.1.0",
    muxviaHome: root,
    fetch: async () => {
      calls += 1
      throw new Error("offline")
    },
  }

  expect(await checkForUpdate({ ...options, now: () => 2_000_000 })).toBeUndefined()
  expect(await checkForUpdate({ ...options, now: () => 2_000_000 + 86_399_999 })).toBeUndefined()
  expect(calls).toBe(1)
  expect(await checkForUpdate({ ...options, now: () => 2_000_000 + 86_400_000 })).toBeUndefined()
  expect(calls).toBe(2)
})

test("is disableable and never reports the current, older, or malformed release", async () => {
  const root = await home()
  let calls = 0
  expect(await checkForUpdate({
    currentRelease: "0.1.0",
    muxviaHome: root,
    environment: { MUXVIA_UPDATE_CHECK: "0" },
    fetch: async () => { calls += 1; return Response.json({}) },
  })).toBeUndefined()
  expect(calls).toBe(0)

  for (const release of ["0.1.0", "0.0.9", "latest"]) {
    const next = await home()
    expect(await checkForUpdate({
      currentRelease: "0.1.0",
      muxviaHome: next,
      now: () => 3_000_000,
      fetch: async () => Response.json({ schemaVersion: 1, product: "muxvia", release }),
    })).toBeUndefined()
  }
})

test("a concurrent process observes the private local lock instead of making a second request", async () => {
  const root = await home()
  let calls = 0
  let release!: () => void
  const held = new Promise<void>((resolve) => { release = resolve })
  const options = {
    currentRelease: "0.1.0",
    muxviaHome: root,
    now: () => 4_000_000,
    fetch: async () => {
      calls += 1
      await held
      return Response.json({ schemaVersion: 1, product: "muxvia", release: "0.2.0" })
    },
  }
  const first = checkForUpdate(options)
  await Bun.sleep(20)
  expect(await checkForUpdate(options)).toBeUndefined()
  release()
  expect(await first).toEqual({ release: "0.2.0" })
  expect(calls).toBe(1)
})
