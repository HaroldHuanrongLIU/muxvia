# Store first-release credentials in local files

To preserve CC-Switch-compatible behavior, the first release will persist provider API credentials and subscription refresh tokens without application-level encryption in local SQLite or JSON storage, protected with best-effort private filesystem permissions. Muxvia will not require macOS Keychain or Linux Secret Service, accepting the resulting backup, malware, and local-file disclosure risk as an explicit product trade-off.
