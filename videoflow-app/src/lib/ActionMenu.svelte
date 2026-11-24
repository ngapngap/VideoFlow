<script context="module" lang="ts">
  export interface ActionMenuItem {
    id: string;
    label: string;
    icon?: string;
    danger?: boolean;
    disabled?: boolean;
    title?: string;
    dividerBefore?: boolean;
  }
</script>

<script lang="ts">
  import { createEventDispatcher, onDestroy } from "svelte";
  import Icons from "./Icons.svelte";

  export let items: ActionMenuItem[] = [];
  export let label = "Thao tác khác";
  export let buttonText = "";
  export let align: "left" | "right" = "right";
  export let direction: "down" | "up" = "down";
  export let disabled = false;

  const dispatch = createEventDispatcher<{ select: string }>();

  let open = false;
  let rootEl: HTMLDivElement | null = null;

  function close() {
    open = false;
  }

  function toggle(e: MouseEvent) {
    e.stopPropagation();
    if (disabled) return;
    open = !open;
  }

  function pick(item: ActionMenuItem) {
    if (item.disabled) return;
    open = false;
    dispatch("select", item.id);
  }

  function onWindowClick(e: MouseEvent) {
    if (!open || !rootEl) return;
    if (!rootEl.contains(e.target as Node)) {
      close();
    }
  }

  function onWindowKeydown(e: KeyboardEvent) {
    if (open && e.key === "Escape") {
      e.stopPropagation();
      close();
    }
  }

  onDestroy(() => {
    open = false;
  });
</script>

<svelte:window on:click={onWindowClick} on:keydown={onWindowKeydown} />

<div class="action-menu-root" bind:this={rootEl}>
  <button
    type="button"
    class="btn-secondary action-menu-trigger"
    class:icon-only={!buttonText}
    aria-haspopup="menu"
    aria-expanded={open}
    aria-label={label}
    title={label}
    {disabled}
    on:click={toggle}
  >
    <Icons name="more-horizontal" size={15} />
    {#if buttonText}
      <span>{buttonText}</span>
    {/if}
  </button>

  {#if open}
    <div
      class="action-menu-popover"
      class:align-left={align === "left"}
      class:align-right={align === "right"}
      class:dir-up={direction === "up"}
      class:dir-down={direction === "down"}
      role="menu"
      aria-label={label}
    >
      {#each items as item (item.id)}
        {#if item.dividerBefore}
          <div class="action-menu-divider" role="separator"></div>
        {/if}
        <button
          type="button"
          class="action-menu-item"
          class:danger={item.danger}
          role="menuitem"
          disabled={item.disabled}
          title={item.title || item.label}
          on:click={() => pick(item)}
        >
          {#if item.icon}
            <Icons name={item.icon} size={14} />
          {/if}
          <span>{item.label}</span>
        </button>
      {/each}
    </div>
  {/if}
</div>

<style>
  .action-menu-root {
    position: relative;
    display: inline-flex;
  }
  .action-menu-trigger {
    height: 34px;
    padding: 0 10px;
    font-size: var(--fs-label);
    gap: 6px;
  }
  .action-menu-trigger.icon-only {
    width: 30px;
    height: 30px;
    padding: 0;
  }
  .action-menu-popover {
    position: absolute;
    z-index: 60;
    min-width: 220px;
    background: var(--c-card);
    border: 1px solid var(--c-border);
    border-radius: 10px;
    box-shadow: var(--shadow-modal);
    padding: 5px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .action-menu-popover.align-right {
    right: 0;
  }
  .action-menu-popover.align-left {
    left: 0;
  }
  .action-menu-popover.dir-down {
    top: calc(100% + 4px);
  }
  .action-menu-popover.dir-up {
    bottom: calc(100% + 6px);
  }
  .action-menu-divider {
    height: 1px;
    background: var(--c-border);
    margin: 4px 6px;
  }
  .action-menu-item {
    width: 100%;
    justify-content: flex-start;
    height: 32px;
    padding: 0 10px;
    font-size: 12.5px;
    font-weight: 500;
    background: transparent;
    color: var(--c-text);
    border-radius: 7px;
    gap: 8px;
  }
  .action-menu-item:hover:not(:disabled) {
    background: var(--c-teal-light);
    color: var(--c-teal);
  }
  .action-menu-item.danger {
    color: var(--c-danger);
  }
  .action-menu-item.danger:hover:not(:disabled) {
    background: var(--c-danger-bg);
    color: var(--c-danger);
  }
</style>
