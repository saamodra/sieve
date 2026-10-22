<script lang="ts">
  interface Props {
    size?: number;
  }
  let { size = 16 }: Props = $props();

  const id = $props.id();
  const HOLES = [
    [16, 16],
    [23.5, 16],
    [19.75, 22.5],
    [12.25, 22.5],
    [8.5, 16],
    [12.25, 9.5],
    [19.75, 9.5],
  ];
</script>

<svg viewBox="0 0 32 32" width={size} height={size} aria-hidden="true">
  <defs>
    <mask id="{id}-m">
      <rect width="32" height="32" fill="#fff" />
      {#each HOLES as [cx, cy] (`${cx},${cy}`)}
        <circle {cx} {cy} r="2.2" fill="#000" />
      {/each}
    </mask>
  </defs>
  <circle cx="16" cy="16" r="15" fill="currentColor" mask="url(#{id}-m)" />
</svg>

<style>
  svg {
    display: block;
    flex: none;
  }
</style>
