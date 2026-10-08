<script lang="ts">
  import { appInfo, EXPECTED_PROTOCOL_VERSION } from "./lib/api";
  import type { AppInfo } from "./lib/protocol/AppInfo";

  let info = $state<AppInfo | null>(null);
  let error = $state<string | null>(null);

  appInfo().then(
    (value) => (info = value),
    (reason: unknown) => (error = String(reason)),
  );
</script>

<main>
  <h1>SpaceBadger</h1>
  {#if error}
    <p role="alert">Backend unavailable: {error}</p>
  {:else if info}
    <p>
      v{info.appVersion} · {info.os}/{info.arch} · protocol {info.protocolVersion}
      {#if info.protocolVersion !== EXPECTED_PROTOCOL_VERSION}
        <strong role="alert">(interface expects {EXPECTED_PROTOCOL_VERSION})</strong>
      {/if}
    </p>
  {:else}
    <p>Connecting…</p>
  {/if}
</main>
