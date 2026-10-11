<script lang="ts">
  import PlusIcon from '$lib/components/icons/PlusIcon.svelte';
  import MicIcon from '$lib/components/icons/MicIcon.svelte';
  import SendIcon from '$lib/components/icons/SendIcon.svelte';

  type AttachmentKind = 'image' | 'video' | 'document' | 'folder';

  let imageInputEl: HTMLInputElement | null = null;
  let videoInputEl: HTMLInputElement | null = null;
  let documentInputEl: HTMLInputElement | null = null;
  let folderInputEl: HTMLInputElement | null = null;

  let {
    body = '',
    attachmentMenuOpen = false,
    selectedAttachments = [],
    isRecording = false,
    submitting = false,
    placeholder = 'Message Consistent…',
    onBodyInput,
    onTextareaInput,
    onTextareaKeydown,
    onToggleAttachmentMenu,
    onOpenAttachmentPicker,
    onAttachmentChange,
    onRemoveAttachment,
    onToggleVoiceRecording,
    onSubmit
  }: {
    body: string;
    attachmentMenuOpen: boolean;
    selectedAttachments: File[];
    isRecording: boolean;
    submitting: boolean;
    placeholder?: string;
    onBodyInput: (next: string) => void;
    onTextareaInput: (event: Event) => void;
    onTextareaKeydown: (event: KeyboardEvent) => void;
    onToggleAttachmentMenu: () => void;
    onOpenAttachmentPicker: (kind: AttachmentKind) => void;
    onAttachmentChange: (event: Event) => void;
    onRemoveAttachment: (index: number) => void;
    onToggleVoiceRecording: () => void;
    onSubmit?: () => void;
  } = $props();

  function openAttachmentPicker(kind: AttachmentKind) {
    onOpenAttachmentPicker(kind);
    if (kind === 'image') imageInputEl?.click();
    if (kind === 'video') videoInputEl?.click();
    if (kind === 'document') documentInputEl?.click();
    if (kind === 'folder') folderInputEl?.click();
  }

  const hasContent = $derived(body.trim().length > 0 || selectedAttachments.length > 0);
</script>

{#if selectedAttachments.length > 0}
  <div class="attachment-chips" aria-label="Selected attachments">
    {#each selectedAttachments as file, idx (`${file.name}-${file.size}-${idx}`)}
      <button type="button" class="attachment-chip" onclick={() => onRemoveAttachment(idx)} title="Remove attachment">
        {file.name}
      </button>
    {/each}
  </div>
{/if}

<form
  class="composer"
  onsubmit={(event) => {
    event.preventDefault();
    if (!hasContent || submitting) return;
    onSubmit?.();
  }}
>
  <div class="composer-attach">
    <button
      type="button"
      class="composer-icon-btn"
      onclick={onToggleAttachmentMenu}
      aria-label="Add attachment"
      title="Add attachment"
      disabled={submitting}
    >
      <PlusIcon size={18} />
    </button>
    {#if attachmentMenuOpen}
      <div class="attachment-menu" role="menu" aria-label="Attachment options">
        <button type="button" class="attachment-menu-item" onclick={() => openAttachmentPicker('image')}>Image</button>
        <button type="button" class="attachment-menu-item" onclick={() => openAttachmentPicker('folder')}>Folder</button>
        <button type="button" class="attachment-menu-item" onclick={() => openAttachmentPicker('document')}>Document</button>
        <button type="button" class="attachment-menu-item" onclick={() => openAttachmentPicker('video')}>Video</button>
      </div>
    {/if}
  </div>

  <textarea
    id="capture-input"
    class="composer-textarea"
    rows="1"
    value={body}
    {placeholder}
    oninput={(event) => {
      onBodyInput((event.currentTarget as HTMLTextAreaElement).value);
      onTextareaInput(event);
    }}
    onkeydown={onTextareaKeydown}
  ></textarea>

  <button
    type="button"
    class="composer-icon-btn mic"
    class:recording={isRecording}
    onclick={onToggleVoiceRecording}
    aria-label={isRecording ? 'Stop recording' : 'Record voice'}
    title={isRecording ? 'Stop recording' : 'Record voice'}
    disabled={submitting}
  >
    <MicIcon size={18} />
  </button>

  <button
    type="submit"
    class="composer-send"
    disabled={!hasContent || submitting}
    aria-label="Send message"
    title="Send message"
  >
    <SendIcon size={22} />
  </button>
</form>

<input bind:this={imageInputEl} class="hidden-file-input" type="file" accept="image/*" multiple onchange={onAttachmentChange} />
<input bind:this={videoInputEl} class="hidden-file-input" type="file" accept="video/*" multiple onchange={onAttachmentChange} />
<input bind:this={documentInputEl} class="hidden-file-input" type="file" accept=".pdf,.doc,.docx,.txt,.md,.rtf,.csv,.ppt,.pptx,.xls,.xlsx" multiple onchange={onAttachmentChange} />
<input bind:this={folderInputEl} class="hidden-file-input" type="file" webkitdirectory multiple onchange={onAttachmentChange} />

<style>
  .attachment-chips {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 8px;
  }
  .attachment-chip {
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-secondary);
    border-radius: 999px;
    font-size: 11px;
    padding: 5px 10px;
    cursor: pointer;
    max-width: 220px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }

  .composer {
    display: flex;
    align-items: flex-end;
    gap: 8px;
    padding: 8px 10px;
    border: 1px solid var(--border);
    border-radius: 16px;
    background: var(--surface-overlay-strong);
    transition: border-color var(--transition-base), box-shadow var(--transition-base);
  }
  .composer:focus-within {
    border-color: rgba(201, 138, 75, 0.45);
    box-shadow: 0 0 0 2px rgba(201, 138, 75, 0.18);
  }

  .composer-attach {
    position: relative;
    flex-shrink: 0;
  }

  .composer-icon-btn {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 38px;
    height: 38px;
    border-radius: 999px;
    border: 1px solid var(--border);
    background: var(--surface-overlay);
    color: var(--text-secondary);
    cursor: pointer;
    transition: background var(--transition-base), color var(--transition-base);
  }
  .composer-icon-btn:hover:not(:disabled) {
    color: var(--text-primary);
    background: var(--surface-hover);
  }
  .composer-icon-btn:disabled {
    opacity: 0.5;
    cursor: not-allowed;
  }
  .composer-icon-btn.mic.recording {
    color: #c96a4b;
    border-color: rgba(201, 106, 75, 0.55);
    background: rgba(201, 106, 75, 0.14);
  }

  .composer-textarea {
    flex: 1;
    min-height: 38px;
    max-height: 220px;
    padding: 10px 8px;
    background: transparent;
    border: 0;
    color: var(--text-primary);
    font-family: inherit;
    font-size: 15px;
    line-height: 1.4;
    resize: none;
    outline: none;
  }
  .composer-textarea::placeholder {
    color: var(--text-muted);
  }

  .composer-send {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 40px;
    height: 40px;
    border-radius: 999px;
    border: 0;
    background: var(--accent-cyan);
    color: #0a0908;
    cursor: pointer;
    transition: background var(--transition-base), transform var(--transition-base);
    flex-shrink: 0;
  }
  .composer-send:hover:not(:disabled) {
    background: #eed9ae;
    transform: translateY(-1px);
  }
  .composer-send:disabled {
    background: rgba(242, 237, 228, 0.18);
    color: var(--text-muted);
    cursor: not-allowed;
  }

  .attachment-menu {
    position: absolute;
    bottom: calc(100% + 8px);
    left: 0;
    z-index: 20;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--bg-card);
    padding: 6px;
    display: grid;
    gap: 4px;
    min-width: 150px;
    box-shadow: 0 12px 28px rgba(0, 0, 0, 0.3);
  }
  .attachment-menu-item {
    border: 1px solid transparent;
    background: transparent;
    color: var(--text-primary);
    border-radius: 8px;
    min-height: 34px;
    padding: 0 10px;
    text-align: left;
    font-size: 12px;
    cursor: pointer;
  }
  .attachment-menu-item:hover {
    border-color: var(--border);
    background: var(--bg-secondary);
  }

  .hidden-file-input {
    display: none;
  }
</style>
