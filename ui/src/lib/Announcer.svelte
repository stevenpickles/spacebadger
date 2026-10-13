<script lang="ts">
  import { announcer } from "./announce.svelte";
</script>

<div class="sr-only" role="status" aria-live="polite" aria-atomic="true">{announcer.text}</div>
