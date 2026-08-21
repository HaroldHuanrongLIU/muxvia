#!/usr/bin/env node

import { runLauncher } from "../lib/launcher.js"

process.exitCode = await runLauncher()
