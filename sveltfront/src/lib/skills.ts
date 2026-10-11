// Skills client — Claude-format procedures the agent loads on demand.
//
// Mirrors `crates/gateway/src/control_api/skills_api.rs` (camelCase on the
// wire). Errors from the skills API carry a JSON `{ error }` body whose text
// is written for people ("a skill named `x` already exists"); `skillErrorMessage`
// pulls it out so pages can show it instead of "HTTP 409".

import { api, ApiError } from '$lib/api';

export type SkillScope = 'builtin' | 'global' | 'entity';

export interface SkillSummary {
  id: string;
  slug: string;
  name: string;
  title: string;
  description: string;
  scope: SkillScope;
  entityId?: string;
  enabled: boolean;
  editable: boolean;
  /** Enabled and not shadowed — what the agent actually sees. */
  active: boolean;
  shadowedBy?: string;
  files: string[];
  updatedAt?: string;
  warnings?: string[];
}

export interface SkillDetail extends SkillSummary {
  body: string;
  raw: string;
}

export interface SkillEntityRef {
  id: string;
  name: string;
}

export interface SkillsListResponse {
  skills: SkillSummary[];
  entities: SkillEntityRef[];
  localImport: boolean;
}

export interface ExternalSkill {
  slug: string;
  name: string;
  description: string;
  path: string;
}

export interface ImportReport {
  imported: SkillSummary[];
  skipped: { source: string; reason: string }[];
}

/** Where a new or copied skill goes: the whole workspace, or one project. */
export type SkillTarget = { scope: 'global' } | { scope: 'entity'; entityId: string };

const enc = encodeURIComponent;

export function listSkills(entityId?: string): Promise<SkillsListResponse> {
  const q = entityId ? `?entityId=${enc(entityId)}` : '';
  return api<SkillsListResponse>(`/api/skills${q}`, { silent: true });
}

export function getSkill(id: string): Promise<SkillDetail> {
  return api<SkillDetail>(`/api/skills/${enc(id)}`, { silent: true });
}

export function createSkill(
  target: SkillTarget,
  input: { name: string; description: string; body: string }
): Promise<SkillSummary> {
  return api<SkillSummary>('/api/skills', {
    method: 'POST',
    silent: true,
    body: JSON.stringify({ ...target, ...input })
  });
}

export function updateSkill(
  id: string,
  patch: { description?: string; body?: string; raw?: string }
): Promise<SkillSummary> {
  return api<SkillSummary>(`/api/skills/${enc(id)}`, {
    method: 'PATCH',
    silent: true,
    body: JSON.stringify(patch)
  });
}

export function deleteSkill(id: string): Promise<{ ok: boolean }> {
  return api<{ ok: boolean }>(`/api/skills/${enc(id)}`, { method: 'DELETE', silent: true });
}

export function setSkillEnabled(id: string, enabled: boolean): Promise<SkillSummary> {
  return api<SkillSummary>(`/api/skills/${enc(id)}/enabled`, {
    method: 'POST',
    silent: true,
    body: JSON.stringify({ enabled })
  });
}

export function copySkill(id: string, target: SkillTarget): Promise<SkillSummary> {
  return api<SkillSummary>(`/api/skills/${enc(id)}/copy`, {
    method: 'POST',
    silent: true,
    body: JSON.stringify(target)
  });
}

export function uploadSkill(
  target: SkillTarget,
  skillMd: string,
  files: { path: string; content: string }[] = [],
  overwrite = false
): Promise<SkillSummary> {
  return api<SkillSummary>('/api/skills/upload', {
    method: 'POST',
    silent: true,
    body: JSON.stringify({ ...target, skillMd, files, overwrite })
  });
}

export function listLocalSkillSources(): Promise<{
  claudeSkillsDir: string | null;
  skills: ExternalSkill[];
}> {
  return api('/api/skills/local', { silent: true });
}

export function importLocalSkills(
  target: SkillTarget,
  paths: string[],
  overwrite = false
): Promise<ImportReport> {
  return api<ImportReport>('/api/skills/local', {
    method: 'POST',
    silent: true,
    body: JSON.stringify({ ...target, paths, overwrite })
  });
}

/** The human sentence inside a skills API error, or a generic fallback. */
export function skillErrorMessage(err: unknown, fallback = 'Something went wrong.'): string {
  if (err instanceof ApiError) {
    try {
      const parsed = JSON.parse(err.message) as { error?: string };
      if (parsed?.error) return parsed.error;
    } catch {
      /* not JSON — fall through */
    }
    if (err.status === 0 || err.message.startsWith('HTTP')) return fallback;
    return err.message;
  }
  if (err instanceof Error && err.message) return err.message;
  return fallback;
}

/** Mirror of the gateway's `slugify`, so the form can preview the name. */
export function slugify(input: string): string {
  return input
    .trim()
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
    .slice(0, 64)
    .replace(/-+$/g, '');
}

export const SKILL_TEMPLATE = `# Skill title

One or two sentences on why this procedure exists — the purpose shapes every judgment call.

## Steps

1. Gather what you need first (for example \`review_today\` or \`recall_recent\`).
2. Do the work, one action per step.
3. Show the result before saving anything.

## Output

\`\`\`
The shape the answer should take.
\`\`\`

## Rules

- What never to do.
- What to always check.
`;

// ─── Markdown, safely ────────────────────────────────────────────────────────

function escapeHtml(s: string): string {
  return s
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

function inline(s: string): string {
  // Input is already escaped; only add tags we generate ourselves.
  return s
    .replace(/`([^`]+)`/g, '<code>$1</code>')
    .replace(/\*\*([^*]+)\*\*/g, '<strong>$1</strong>')
    .replace(/(^|[\s(])\*([^*\s][^*]*)\*/g, '$1<em>$2</em>');
}

/**
 * A small Markdown renderer for skill instructions: headings, lists, fenced
 * code, quotes, tables as text, inline code/bold/italics. Every character of
 * input is HTML-escaped before any tag is added, so a skill imported from
 * anywhere can't inject markup into the dashboard. Links render as text.
 */
export function renderSkillMarkdown(md: string): string {
  const lines = md.replace(/\r\n/g, '\n').split('\n');
  const out: string[] = [];
  let list: 'ul' | 'ol' | null = null;
  let para: string[] = [];
  let fence: string[] | null = null;

  const flushPara = () => {
    if (para.length) {
      out.push(`<p>${inline(escapeHtml(para.join(' ')))}</p>`);
      para = [];
    }
  };
  const closeList = () => {
    if (list) {
      out.push(`</${list}>`);
      list = null;
    }
  };

  for (const line of lines) {
    if (fence) {
      if (/^\s*```/.test(line)) {
        out.push(`<pre><code>${escapeHtml(fence.join('\n'))}</code></pre>`);
        fence = null;
      } else {
        fence.push(line);
      }
      continue;
    }
    if (/^\s*```/.test(line)) {
      flushPara();
      closeList();
      fence = [];
      continue;
    }
    const heading = /^(#{1,4})\s+(.*)$/.exec(line);
    if (heading) {
      flushPara();
      closeList();
      const level = Math.min(heading[1].length + 1, 5);
      out.push(`<h${level}>${inline(escapeHtml(heading[2].trim()))}</h${level}>`);
      continue;
    }
    const bullet = /^\s*[-*]\s+(.*)$/.exec(line);
    const numbered = /^\s*\d+[.)]\s+(.*)$/.exec(line);
    if (bullet || numbered) {
      flushPara();
      const kind = bullet ? 'ul' : 'ol';
      if (list !== kind) {
        closeList();
        out.push(`<${kind}>`);
        list = kind;
      }
      out.push(`<li>${inline(escapeHtml((bullet ?? numbered)![1]))}</li>`);
      continue;
    }
    if (/^\s*>\s?/.test(line)) {
      flushPara();
      closeList();
      out.push(`<blockquote>${inline(escapeHtml(line.replace(/^\s*>\s?/, '')))}</blockquote>`);
      continue;
    }
    if (!line.trim()) {
      flushPara();
      closeList();
      continue;
    }
    if (list && /^\s{2,}\S/.test(line)) {
      // Continuation of the previous list item.
      const last = out.pop() ?? '';
      out.push(last.replace(/<\/li>$/, ` ${inline(escapeHtml(line.trim()))}</li>`));
      continue;
    }
    closeList();
    para.push(line.trim());
  }
  if (fence) out.push(`<pre><code>${escapeHtml(fence.join('\n'))}</code></pre>`);
  flushPara();
  closeList();
  return out.join('\n');
}
