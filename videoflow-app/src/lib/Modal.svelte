<script context="module" lang="ts">
  const modalStack: symbol[] = [];
</script>

<script lang="ts">
  import { createEventDispatcher, onDestroy, tick } from "svelte";
  import { fade, scale } from "svelte/transition";
  import Icons from "./Icons.svelte";

  export let open = false;
  export let title = "";
  export let width = "440px";
  export let disableClose = false;

  const dispatch = createEventDispatcher<{ close: void }>();
  const modalId = Symbol("modal");

  let dialogEl: HTMLDivElement | null = null;
  let previouslyFocused: HTMLElement | null = null;
  let wasOpen = false;

  function pushStack() {
    if (!modalStack.includes(modalId)) {
      modalStack.push(modalId);
    }
  }

  function popStack() {
    const idx = modalStack.indexOf(modalId);
    if (idx !== -1) {
      modalStack.splice(idx, 1);
    }
  }

  function isTopmost(): boolean {
    return modalStack.length > 0 && modalStack[modalStack.length - 1] === modalId;
  }

  function getFocusableElements(): HTMLElement[] {
    if (!dialogEl) return [];
    const nodes = dialogEl.querySelectorAll<HTMLElement>(
      'button:not(:disabled), [href], input:not(:disabled), select:not(:disabled), textarea:not(:disabled), [tabindex]:not([tabindex="-1"])'
    );
    return Array.from(nodes).filter((el) => el.offsetParent !== null);
  }

  async function handleOpenChange(isOpen: boolean) {
    if (isOpen && !wasOpen) {
      wasOpen = true;
      previouslyFocused =
        typeof document !== "undefined" ? (document.activeElement as HTMLElement | null) : null;
      pushStack();
      await tick();
      const focusables = getFocusableElements();
      const preferred =
        focusables.find((el) => !el.classList.contains("modal-close")) ?? focusables[0];
      if (preferred) {
        preferred.focus();
      } else if (dialogEl) {
        dialogEl.focus();
      }
    } else if (!isOpen && wasOpen) {
      wasOpen = false;
      popStack();
      const toRestore = previouslyFocused;
      previouslyFocused = null;
      if (toRestore && typeof toRestore.focus === "function" && toRestore.isConnected) {
        await tick();
        toRestore.focus();
      }
    }
  }

  $: handleOpenChange(open);

  onDestroy(() => {
    popStack();
  });

  function requestClose() {
    if (disableClose) return;
    dispatch("close");
  }

  function onKeydown(e: KeyboardEvent) {
    if (!open || !isTopmost()) return;

    if (e.key === "Escape") {
      e.preventDefault();
      e.stopImmediatePropagation();
      requestClose();
      return;
    }

    if (e.key === "Tab" && dialogEl) {
      const focusables = getFocusableElements();
      if (focusables.length === 0) {
        e.preventDefault();
        return;
      }
      const first = focusables[0];
      const last = focusables[focusables.length - 1];
      const active = document.activeElement as HTMLElement | null;
      if (e.shiftKey) {
        if (!active || active === first || !dialogEl.contains(active)) {
          e.preventDefault();
          last.focus();
        }
      } else {
        if (!active || active === last || !dialogEl.contains(active)) {
          e.preventDefault();
          first.focus();
        }
      }
    }
  }
</script>

<svelte:window on:keydown={onKeydown} />

{#if open}
  <div
    class="modal-backdrop"
    transition:fade={{ duration: 140 }}
    on:click|self={requestClose}
    role="presentation"
  >
    <div
      bind:this={dialogEl}
      class="modal-dialog"
      style="max-width: {width};"
      role="dialog"
      aria-modal="true"
      aria-label={title || "Hộp thoại"}
      tabindex="-1"
      transition:scale={{ start: 0.96, duration: 150 }}
    >
      {#if title}
        <div class="modal-head">
          <h2 class="modal-title">{title}</h2>
          <button
            type="button"
            class="btn-ghost modal-close"
            aria-label="Đóng hộp thoại"
            disabled={disableClose}
            on:click={requestClose}
          >
            <Icons name="x" size={16} />
          </button>
        </div>
      {/if}

      <div class="modal-body">
        <slot />
      </div>

      {#if $$slots.footer}
        <div class="modal-foot">
          <slot name="footer" />
        </div>
      {/if}
    </div>
  </div>
{/if}

<style>
  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: rgba(15, 23, 42, 0.45);
    backdrop-filter: blur(2px);
    display: flex;
    align-items: center;
    justify-content: center;
    padding: 20px;
    z-index: 100;
  }
  .modal-dialog {
    width: 100%;
    max-height: calc(100vh - 48px);
    background: var(--c-card);
    border: 1px solid var(--c-border);
    border-radius: var(--r-modal);
    box-shadow: var(--shadow-modal);
    display: flex;
    flex-direction: column;
    overflow: hidden;
    outline: none;
  }
  .modal-head {
    padding: 14px 18px;
    border-bottom: 1px solid var(--c-border);
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
  }
  .modal-title {
    font-size: var(--fs-h-block);
    font-weight: 700;
    color: var(--c-text);
  }
  .modal-close {
    width: 30px;
    height: 30px;
    padding: 0;
    border-radius: 6px;
  }
  .modal-body {
    padding: 16px 18px;
    overflow-y: auto;
    flex: 1;
    font-size: var(--fs-body);
    color: var(--c-text);
  }
  .modal-foot {
    padding: 12px 18px;
    border-top: 1px solid var(--c-border);
    background: var(--c-surface-subtle);
    display: flex;
    align-items: center;
    justify-content: flex-end;
    gap: 8px;
  }
</style>
