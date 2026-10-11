<script lang="ts">
  import Seo from '$lib/components/Seo.svelte';

  const audiencePillars = [
    {
      title: 'Proactive assistant first',
      body:
        'Consistent CEO goes to market as your AI assistant. It reminds you, speaks up at the right time, keeps context, and helps you reply or act without making you think about the architecture underneath.'
    },
    {
      title: 'Local-first by design',
      body:
        'The product is designed to run on your own device, a dedicated work machine, or a controlled node. Mobile and desktop can both exist, with lightweight free usage and a paid upgrade path for stronger models, more automations, and more orchestration.'
    },
    {
      title: 'Templates before complexity',
      body:
        'Users should start with Consistent CEO plus a few official workflows. Over time they can clone and adapt director templates for their own industry, team, and operating style.'
    }
  ];

  const nonTechnicalSections = [
    {
      title: 'What Consistent CEO is',
      body:
        'Consistent CEO is a proactive AI assistant that remembers your work, nudges you at the right time, and helps you run projects through built-in specialist helpers behind the scenes.'
    },
    {
      title: 'How people experience it',
      body:
        'A notification arrives, you tap it, Consistent CEO reads it out, and you reply naturally. It can then save a note, create a task, schedule a reminder, or hand work to the right specialist flow.'
    },
    {
      title: 'Why it is different',
      body:
        'Most assistants only react. Consistent CEO stays aware of ongoing work through memory, entity context, and heartbeat reviews, so it can follow up, remind, and keep work moving.'
    },
    {
      title: 'Who it is for first',
      body:
        'The best first users are solo operators, founders, builders, and high-agency people who want an assistant that helps them run work across a trusted machine or node.'
    }
  ];

  const technicalSections = [
    {
      title: 'Operating model',
      body:
        'Consistent CEO stays simple at the surface, but underneath it uses an operating layer that interprets input, routes work, and coordinates specialist directors. Operations is the default intake layer.'
    },
    {
      title: 'Entity memory',
      body:
        'Each entity accumulates purpose, assets, goals, readiness, history, and execution context. This is what lets the assistant continue work without recreating the same context every time.'
    },
    {
      title: 'Heartbeat and NTT loop',
      body:
        'Heartbeat is the recurring loop that keeps work alive. A good way to describe the internal rhythm is note, task, and trigger, supported by heartbeat reviews that decide what should be followed up, queued, or executed next.'
    },
    {
      title: 'Execution surface',
      body:
        'Desktop is the strongest first execution surface because it can coordinate local tools, files, notifications, and runtime services. Mobile can be a lighter front-end companion and free entry point, especially when paired with local models or paid orchestration.'
    },
    {
      title: 'Security posture',
      body:
        'The correct posture is local-first and user-controlled, not “fully secure” as a blanket claim. Sensitive automation should run on a dedicated machine, VPS, or controlled node, with explicit warnings and approvals for risky actions.'
    },
    {
      title: 'Future extensibility',
      body:
        'Directors and workflows should begin as official templates. Later, users can copy and adapt them. Custom creation can come after the core safety, reliability, and orchestration model are stable.'
    }
  ];

  const roadmap = [
    'Make Consistent CEO excellent for one user on one trusted machine first.',
    'Ship a mobile and desktop story with clear free versus paid usage.',
    'Keep the assistant experience simple, while templates unlock deeper workflows.',
    'Use the knowledge hub as the product compass so docs and implementation stay aligned.'
  ];

  // Answer-engine grounding: the non-technical and technical sections above, restated as
  // a schema.org FAQPage so crawlers get the same answers without executing the app.
  const faqSchema = {
    '@context': 'https://schema.org',
    '@type': 'FAQPage',
    name: 'Consistent CEO — Knowledge Hub',
    url: 'https://consistent.ceo/about',
    mainEntity: [...nonTechnicalSections, ...technicalSections].map((section) => ({
      '@type': 'Question',
      name: section.title,
      acceptedAnswer: { '@type': 'Answer', text: section.body }
    }))
  };
</script>

<Seo
  title="Knowledge Hub"
  description="What Consistent CEO is, who it is for, and how it works — a proactive, local-first AI assistant that remembers your work, follows up on it, and routes it to specialist directors under human oversight."
  canonical="/about"
/>

<!-- FAQ structured data. This is the page LLM crawlers and answer engines are most
     likely to ground on, so the same Q&A shown below is also emitted machine-readably. -->
<svelte:head>
  {@html `<script type="application/ld+json">${JSON.stringify(faqSchema)}<\/script>`}
</svelte:head>

<div class="knowledge-hub">
  <section class="hero">
    <div class="eyebrow">Knowledge Hub</div>
    <h1>Consistent CEO, your AI assistant</h1>
    <p class="hero-copy">
      Consistent CEO should feel simple on the surface: a proactive assistant that remembers your work,
      notifies you, talks back, and quietly orchestrates specialist help underneath.
    </p>
    <div class="hero-actions">
      <a class="btn primary" href="/download">Download the app</a>
      <a class="btn" href="/">Open workspace</a>
      <a class="btn ghost" href="https://github.com/Al-Baqshi/Consistent-world" target="_blank" rel="noreferrer">GitHub</a>
    </div>
  </section>

  <section class="pillar-grid" aria-label="Product pillars">
    {#each audiencePillars as pillar}
      <article class="pillar-card">
        <h2>{pillar.title}</h2>
        <p>{pillar.body}</p>
      </article>
    {/each}
  </section>

  <section class="hub-grid">
    <article class="hub-panel non-technical">
      <div class="section-label">Non-technical</div>
      <h2>What this is about</h2>
      <p class="section-intro">
        This section explains the product in human terms, for users, partners, and anyone who wants
        to understand the experience without thinking about internals.
      </p>
      <div class="stack">
        {#each nonTechnicalSections as section}
          <div class="stack-card">
            <h3>{section.title}</h3>
            <p>{section.body}</p>
          </div>
        {/each}
      </div>
    </article>

    <article class="hub-panel technical">
      <div class="section-label">Technical</div>
      <h2>How the system stays on track</h2>
      <p class="section-intro">
        This section is for builders and technical collaborators. It describes the local-first model,
        director orchestration, entity memory, heartbeat, and the internal structure we want to keep
        consistent as the product matures.
      </p>
      <div class="stack">
        {#each technicalSections as section}
          <div class="stack-card">
            <h3>{section.title}</h3>
            <p>{section.body}</p>
          </div>
        {/each}
      </div>
    </article>
  </section>

  <section class="roadmap-panel">
    <div class="section-label">Product direction</div>
    <h2>How people should see it</h2>
    <p class="section-intro">
      The public story should stay clear: Consistent CEO is the assistant. Directors, templates, heartbeats,
      and entity memory are the power layer underneath.
    </p>
    <ol>
      {#each roadmap as item}
        <li>{item}</li>
      {/each}
    </ol>
    <div class="note-box">
      Later, you can add demo photos, live explainer clips, and product walkthroughs here without
      changing the structure of the hub.
    </div>
  </section>
</div>

<style>
  .knowledge-hub {
    max-width: 1180px;
    margin: 0 auto;
    padding: 22px 16px 48px;
    display: grid;
    gap: 20px;
    background:
      radial-gradient(760px 320px at 15% 0%, rgba(201, 138, 75, 0.16), transparent 62%),
      radial-gradient(840px 360px at 100% 12%, rgba(201, 138, 75, 0.12), transparent 60%),
      radial-gradient(900px 420px at 50% 100%, rgba(201, 138, 75, 0.08), transparent 70%);
  }

  .hero,
  .pillar-card,
  .hub-panel,
  .roadmap-panel {
    border: 1px solid var(--border);
    border-radius: 18px;
    background: var(--panel-gradient);
    box-shadow: var(--shadow-md);
  }

  .hero {
    padding: 24px;
    display: grid;
    gap: 14px;
  }

  .eyebrow,
  .section-label {
    display: inline-flex;
    width: fit-content;
    align-items: center;
    gap: 6px;
    min-height: 28px;
    padding: 0 12px;
    border-radius: 999px;
    border: 1px solid rgba(201, 138, 75, 0.24);
    background: var(--surface-accent-soft);
    color: var(--accent-cyan);
    font-size: 11px;
    font-weight: 800;
    letter-spacing: 0.06em;
    text-transform: uppercase;
  }

  h1 {
    margin: 0;
    font-size: clamp(2rem, 4vw, 3.4rem);
    line-height: 1.02;
    letter-spacing: -0.04em;
    color: var(--heading-contrast);
  }

  .hero-copy,
  .section-intro,
  .pillar-card p,
  .stack-card p,
  .note-box,
  ol {
    margin: 0;
    color: var(--text-secondary);
    line-height: 1.7;
    font-size: 14px;
  }

  .hero-copy {
    max-width: 68ch;
    font-size: 16px;
  }

  .hero-actions {
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
  }

  .btn {
    min-height: 44px;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    padding: 0 16px;
    border-radius: 12px;
    border: 1px solid var(--border);
    color: var(--text-primary);
    text-decoration: none;
    background: var(--surface-overlay);
    font-size: 13px;
    font-weight: 700;
  }

  .btn.primary {
    background: linear-gradient(180deg, rgba(201, 138, 75, 0.85), rgba(67, 56, 202, 0.95));
    border-color: rgba(129, 140, 248, 0.7);
    color: white;
  }

  .btn.ghost {
    background: transparent;
  }

  .pillar-grid {
    display: grid;
    gap: 14px;
    grid-template-columns: repeat(auto-fit, minmax(220px, 1fr));
  }

  .pillar-card {
    padding: 18px;
    display: grid;
    gap: 8px;
  }

  .pillar-card h2,
  .hub-panel h2,
  .roadmap-panel h2 {
    margin: 0;
    font-size: 20px;
    color: var(--text-primary);
  }

  .hub-grid {
    display: grid;
    gap: 16px;
    grid-template-columns: 1fr;
  }

  .hub-panel,
  .roadmap-panel {
    padding: 20px;
    display: grid;
    gap: 12px;
  }

  .stack {
    display: grid;
    gap: 12px;
  }

  .stack-card {
    border: 1px solid var(--border-subtle);
    border-radius: 14px;
    background: var(--stack-card-bg);
    padding: 14px;
    display: grid;
    gap: 6px;
  }

  .stack-card h3 {
    margin: 0;
    font-size: 15px;
    color: var(--heading-contrast);
  }

  ol {
    padding-left: 1.2rem;
    display: grid;
    gap: 8px;
  }

  .note-box {
    border-radius: 14px;
    border: 1px dashed rgba(201, 138, 75, 0.28);
    padding: 14px;
    background: var(--surface-accent-soft);
  }

  @media (min-width: 900px) {
    .knowledge-hub {
      padding: 28px 22px 56px;
      gap: 24px;
    }

    .hero {
      padding: 30px;
    }

    .hub-grid {
      grid-template-columns: 1fr 1fr;
      align-items: start;
    }
  }
</style>
