<script lang="ts">
  // A file system path that wraps after its separators rather than in the
  // middle of a folder name.

  interface Props {
    path: string;
  }

  let { path }: Props = $props();

  const parts = $derived(path.split(/(?<=[\\/])/));
</script>

<span class="path-text">{#each parts as part, i (i)}{part}<wbr />{/each}</span>

<style>
  .path-text {
    overflow-wrap: break-word;
  }
</style>
