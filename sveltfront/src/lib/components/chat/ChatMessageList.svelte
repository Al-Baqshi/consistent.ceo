<script lang="ts" module>
  export type ChatAttachmentRef = {
    id: string;
    filename: string;
    content_type: string;
    size_bytes: number;
  };

  /** Evidence states from docs/VISION.md, as the gateway sends them
   *  (snake_case, matching every other kind that crosses the wire). */
  export type EvidenceState =
    | 'verified'
    | 'observed'
    | 'inferred'
    | 'unverified'
    | 'not_applicable';

  export type ChatEvidence = {
    state: EvidenceState;
    /** Tools that grounded the answer. Absent when nothing did. */
    tools?: string[];
  };

  export type ChatBubble = {
    id: string;
    role: 'assistant' | 'user' | 'system';
    text: string;
    tone?: 'default' | 'success' | 'question' | 'creating-entity' | 'heartbeat';
    meta?: string;
    attachments?: ChatAttachmentRef[];
    evidence?: ChatEvidence | null;
  };
</script>

<script lang="ts">
  import { tick } from 'svelte';

  // Wording follows VISION.md's table. "Inferred" is shown as plainly as
  // "Observed": the point is that an ungrounded answer says so itself, rather
  // than looking identical to a grounded one.
  const EVIDENCE_LABEL: Record<EvidenceState, string> = {
    verified: 'Verified',
    observed: 'Observed',
    inferred: 'Inferred',
    unverified: 'Unverified',
    not_applicable: ''
  };

  function evidenceTitle(e: ChatEvidence): string {
    if (e.state === 'observed') {
      const tools = e.tools?.length ? ` (${e.tools.join(', ')})` : '';
      return `Backed by a tool result from this turn${tools}`;
    }
    if (e.state === 'verified') return 'Resolved against your workspace';
    if (e.state === 'inferred') return 'The model\u2019s own reasoning — nothing looked it up';
    if (e.state === 'unverified') return 'This claim did not resolve to anything on file';
    return '';
  }
  import SparkleIcon from '$lib/components/icons/SparkleIcon.svelte';
  import AttachmentImage from './AttachmentImage.svelte';

  let { chatHistory = [] }: { chatHistory: ChatBubble[] } = $props();

  let scrollArea: HTMLDivElement | null = $state(null);
  let isAtBottom = $state(true);
  let lastCount = 0;
  let lastTailLength = 0;

  function getFileIcon(contentType: string): string {
    if (contentType.startsWith('video/')) return '🎬';
    if (contentType.startsWith('audio/')) return '🎵';
    if (contentType.includes('pdf')) return '📄';
    if (contentType.includes('spreadsheet') || contentType.includes('csv') || contentType.includes('excel')) return '📊';
    if (contentType.includes('presentation') || contentType.includes('powerpoint')) return '📑';
    if (contentType.includes('zip') || contentType.includes('tar') || contentType.includes('archive')) return '📦';
    if (contentType.includes('text') || contentType.includes('markdown')) return '📝';
    return '📎';
  }

  function onScroll(event: Event) {
    const el = event.currentTarget as HTMLDivElement;
    const gap = el.scrollHeight - el.clientHeight - el.scrollTop;
    isAtBottom = gap < 24;
  }

  async function scrollToBottom() {
    await tick();
    if (!scrollArea) return;
    scrollArea.scrollTo({ top: scrollArea.scrollHeight, behavior: 'smooth' });
  }

  $effect(() => {
    // Track the tail bubble's length too, so a reply streaming into an
    // existing bubble keeps the view pinned to the bottom, not just new rows.
    const count = chatHistory.length;
    const tailLength = count > 0 ? chatHistory[count - 1].text.length : 0;
    if (count !== lastCount || tailLength !== lastTailLength) {
      const grew = count > lastCount || (count === lastCount && tailLength > lastTailLength);
      lastCount = count;
      lastTailLength = tailLength;
      if (grew && isAtBottom) {
        void scrollToBottom();
      }
    }
  });
</script>

<div class="chat-scroll" bind:this={scrollArea} onscroll={onScroll} aria-label="Conversation">
  <div class="chat-history">
    {#each chatHistory as bubble (bubble.id)}
      <article class={`chat-row ${bubble.role}`}>
        {#if bubble.role === 'assistant'}
          <div class="avatar assistant" aria-hidden="true">
            <SparkleIcon size={14} />
          </div>
        {/if}
        <div class={`chat-bubble ${bubble.role} ${bubble.tone ?? 'default'}`}>
          {#if bubble.meta || (bubble.evidence && bubble.evidence.state !== 'not_applicable')}
            <div class="chat-bubble-meta">
              {#if bubble.meta}<span>{bubble.meta}</span>{/if}
              {#if bubble.evidence && bubble.evidence.state !== 'not_applicable'}
                <span
                  class={`evidence-chip ${bubble.evidence.state}`}
                  title={evidenceTitle(bubble.evidence)}
                >
                  {EVIDENCE_LABEL[bubble.evidence.state]}
                </span>
              {/if}
            </div>
          {/if}
          {#if bubble.attachments && bubble.attachments.length > 0}
            <div class="chat-attachments">
              {#each bubble.attachments as att (att.id)}
                {#if att.content_type.startsWith('image/')}
                  <AttachmentImage class="attachment-image" id={att.id} alt={att.filename} />
                {:else}
                  <div class="attachment-file">
                    <span class="attachment-file-icon">{getFileIcon(att.content_type)}</span>
                    <span class="attachment-file-name">{att.filename}</span>
                  </div>
                {/if}
              {/each}
            </div>
          {/if}
          <p>{bubble.text}</p>
        </div>
      </article>
    {/each}
  </div>
</div>

<style>
  .chat-scroll {
    flex: 1 1 0;
    min-height: 0;
    overflow-y: auto;
    overscroll-behavior: contain;
    padding: 16px 8px 12px;
  }
  .chat-history {
    display: flex;
    flex-direction: column;
    gap: 12px;
    max-width: 820px;
    margin: 0 auto;
  }
  .chat-row {
    display: flex;
    align-items: flex-end;
    gap: 8px;
  }
  .chat-row.user {
    justify-content: flex-end;
  }
  .chat-row.assistant,
  .chat-row.system {
    justify-content: flex-start;
  }

  .avatar {
    flex-shrink: 0;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    border-radius: 999px;
    background: rgba(201, 138, 75, 0.15);
    color: var(--accent-cyan);
    border: 1px solid rgba(201, 138, 75, 0.35);
  }

  .chat-bubble {
    padding: 10px 14px;
    border-radius: 16px;
    max-width: min(78%, 620px);
    line-height: 1.5;
    font-size: 14px;
  }
  .chat-bubble p {
    margin: 0;
    white-space: pre-wrap;
    word-wrap: break-word;
  }
  .chat-bubble-meta {
    font-size: 10px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-muted);
    margin-bottom: 4px;
    font-weight: 700;
    display: flex;
    align-items: center;
    gap: 6px;
    flex-wrap: wrap;
  }

  .evidence-chip {
    border: 1px solid var(--border-subtle);
    border-radius: 999px;
    padding: 1px 6px;
    font-size: 9px;
    letter-spacing: 0.06em;
    color: var(--text-muted);
    cursor: help;
  }

  /* Grounded states get a colour; Inferred stays deliberately quiet so the
     chip reads as information, not as a warning on every ordinary reply. */
  .evidence-chip.observed,
  .evidence-chip.verified {
    color: var(--accent-cyan);
    border-color: color-mix(in srgb, var(--accent-cyan) 45%, transparent);
  }

  .evidence-chip.unverified {
    color: var(--text-primary);
    border-color: color-mix(in srgb, var(--text-primary) 35%, transparent);
  }

  .chat-bubble.assistant,
  .chat-bubble.system {
    background: var(--chat-assistant-bg);
    color: var(--text-primary);
    border: 1px solid var(--chat-assistant-border);
    border-top-left-radius: 6px;
  }

  .chat-bubble.user {
    background: var(--chat-user-bg);
    color: var(--chat-user-text);
    border-top-right-radius: 6px;
  }
  .chat-bubble.user .chat-bubble-meta {
    color: rgba(11, 23, 32, 0.6);
  }

  .chat-bubble.question {
    border-color: rgba(224, 165, 94, 0.45);
  }
  .chat-bubble.success {
    border-color: rgba(201, 138, 75, 0.55);
  }
  .chat-bubble.creating-entity {
    border-color: rgba(74, 61, 82, 0.55);
    background: linear-gradient(180deg, rgba(74, 61, 82, 0.12), rgba(138, 90, 46, 0.06));
    box-shadow: 0 0 0 1px rgba(74, 61, 82, 0.15);
  }
  .chat-bubble.user.creating-entity {
    background: linear-gradient(180deg, rgba(74, 61, 82, 0.20), rgba(138, 90, 46, 0.10));
    color: var(--text-primary);
  }
  .chat-bubble.user.creating-entity .chat-bubble-meta {
    color: var(--text-muted);
  }
  /* Phase 9: heartbeat-result bubbles. Tinted with the active entity colour
     (via --entity-tint on .chat-page) so a heartbeat for the QS Me entity is
     visibly framed in QS Me's accent, distinct from a regular system message. */
  .chat-bubble.heartbeat {
    border-color: color-mix(in srgb, var(--entity-tint, #c98a4b) 60%, transparent);
    background: linear-gradient(
      180deg,
      color-mix(in srgb, var(--entity-tint, #c98a4b) 14%, transparent),
      color-mix(in srgb, var(--entity-tint, #c98a4b) 6%, transparent)
    );
    box-shadow: 0 0 0 1px color-mix(in srgb, var(--entity-tint, #c98a4b) 22%, transparent);
  }
  .chat-bubble.heartbeat .chat-bubble-meta {
    font-weight: 700;
    letter-spacing: 0.02em;
    color: color-mix(in srgb, var(--entity-tint, #c98a4b) 60%, var(--text-primary));
  }

  .chat-attachments {
    display: flex;
    flex-wrap: wrap;
    gap: 6px;
    margin-bottom: 6px;
  }
  .attachment-image {
    max-width: 200px;
    max-height: 150px;
    border-radius: 8px;
    object-fit: cover;
    border: 1px solid var(--border-subtle);
  }
  .attachment-file {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 10px;
    border-radius: 8px;
    background: rgba(0, 0, 0, 0.15);
    font-size: 12px;
    border: 1px solid var(--border-subtle);
  }
  .chat-bubble.user .attachment-file {
    background: rgba(11, 23, 32, 0.12);
  }
  .attachment-file-icon {
    font-size: 14px;
  }
  .attachment-file-name {
    max-width: 160px;
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
  }
</style>
