# Codex Subscription Bridge

Muxvia can expose a ChatGPT Codex subscription account to Claude Code through a Claude-only Target Provider. Claude Code connects to Muxvia's local authenticated Takeover listener with the Anthropic Messages protocol. Muxvia converts supported text and tool traffic to the fixed `https://chatgpt.com/backend-api/codex` endpoint and converts the streaming response back to Anthropic events.

This bridge uses an undocumented ChatGPT Codex interface derived from the pinned CC-Switch v3.19.2 compatibility baseline. It is not officially supported or endorsed by OpenAI or Anthropic, may stop working without notice, and may be subject to applicable account and subscription terms. Requests consume the selected account's shared subscription quota. Muxvia never presents this as an official API integration.

The Provider is credentialless: it must use the fixed endpoint, `codex-subscription` authentication, Claude Takeover, and a Subscription Account binding. Select either an exact Fixed account or Follow Default in the Subscription Accounts workflow. Missing accounts, no default account, Needs Reauthorization, and refresh failure make only that Provider attempt fail; Muxvia never substitutes another account identity. A later eligible Provider in the Target failover route may still run.

The pinned release tests `gpt-5.6` and `gpt-5.6-luna` for text and tool traffic. This is not a claim that arbitrary model names or capabilities work.

## Compatibility Deviations

- The bridge has no `/v1/messages/count_tokens` mapping and returns a fixed local `501` without contacting an account or upstream.
- It makes no image, PDF, or other multimodal conversion claim.
- It does not emulate WebSearch.
- It does not expose a FAST or service-tier toggle.
- It does not project quota, a model catalog, `prompt-cache-key`, usage prices, or cached-token pricing.
- It does not reproduce CC-Switch model alias or role mapping beyond the Provider's single configured model.
- It makes no arbitrary model compatibility claim.
- It uses stricter bounded JSON and SSE parsing with secret-free fixed diagnostics.
- Provider failover changes the Serving Provider only; it does not rewrite the Current Target Provider.

Unsupported features fail locally or remain explicitly unclaimed rather than being silently approximated.
