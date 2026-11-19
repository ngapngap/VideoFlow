<script lang="ts">
  import { fly } from "svelte/transition";
  import { dismissToast, pauseToast, resumeToast, toasts, type ToastType } from "./store";
  import Icons from "./Icons.svelte";

  function iconFor(type: ToastType): string {
    switch (type) {
      case "success":
        return "check";
      case "warn":
        return "alert";
      case "error":
        return "alert";
      default:
        return "info";
    }
  }
</script>

{#if $toasts.length > 0}
  <div class="toast-stack" aria-live="polite">
    {#each $toasts as t (t.id)}
      <div
        class="toast toast-{t.type}"
        transition:fly={{ x: 24, duration: 180 }}
        on:mouseenter={() => pauseToast(t.id)}
        on:mouseleave={() => resumeToast(t.id)}
        role="status"
      >
        <span class="toast-icon">
          <Icons name={iconFor(t.type)} size={16} />
        </span>
        <div class="toast-msg">{t.message}</div>
        <button
          type="button"
          class="btn-ghost toast-dismiss"
          aria-label="Đóng thông báo"
          on:click={() => dismissToast(t.id)}
        >
          <Icons name="x" size={14} />
        </button>
      </div>
    {/each}
  </div>
{/if}

<style>
  .toast-stack {
    position: fixed;
    bottom: 40px;
    right: 16px;
    width: 360px;
    max-width: calc(100vw - 32px);
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 200;
    pointer-events: none;
  }
  .toast {
    pointer-events: auto;
    background: var(--c-card);
    border: 1px solid var(--c-border);
    border-left: 4px solid var(--c-teal);
    border-radius: 10px;
    box-shadow: var(--shadow-hover);
    padding: 10px 10px 10px 12px;
    display: flex;
    align-items: flex-start;
    gap: 10px;
    user-select: text;
  }
  .toast-success {
    border-left-color: var(--c-green);
  }
  .toast-success .toast-icon {
    color: var(--c-green);
    background: var(--c-green-bg);
  }
  .toast-warn {
    border-left-color: var(--c-warn);
  }
  .toast-warn .toast-icon {
    color: var(--c-warn-dark);
    background: var(--c-warn-bg);
  }
  .toast-error {
    border-left-color: var(--c-danger);
  }
  .toast-error .toast-icon {
    color: var(--c-danger);
    background: var(--c-danger-bg);
  }
  .toast-info .toast-icon {
    color: var(--c-teal);
    background: var(--c-teal-light);
  }
  .toast-icon {
    width: 24px;
    height: 24px;
    border-radius: 6px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    flex-shrink: 0;
    margin-top: 1px;
  }
  .toast-msg {
    flex: 1;
    font-size: var(--fs-label);
    line-height: 1.4;
    color: var(--c-text);
    word-break: break-word;
    padding-top: 2px;
  }
  .toast-dismiss {
    width: 24px;
    height: 24px;
    padding: 0;
    border-radius: 6px;
    flex-shrink: 0;
  }
</style>
