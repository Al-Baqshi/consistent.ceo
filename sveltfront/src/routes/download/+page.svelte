<script lang="ts">
  import { onMount } from 'svelte';
  import Seo from '$lib/components/Seo.svelte';
  import VaultIcon from '$lib/components/icons/VaultIcon.svelte';
  import { BUILDS, RELEASES_URL, SOURCE_URL, detectMacArch, detectOs, type PlatformId } from '$lib/download';

  // Featured platform, refined once we can read the browser on mount.
  let primary = $state<PlatformId>('macos-arm');
  let osLabel = $state('your device');
  // Let a Mac visitor flip between Apple Silicon and Intel.
  let macArch = $state<'macos-arm' | 'macos-intel'>('macos-arm');
  // Whether the chip above was actually detected or is still the default guess.
  let archConfirmed = $state(false);

  onMount(() => {
    const os = detectOs();
    primary = os.primary;
    osLabel = os.osLabel;
    if (os.primary === 'macos-arm' || os.primary === 'macos-intel') {
      // Async, so it lands a tick after the first paint; until then the button
      // shows the Apple Silicon default.
      void detectMacArch().then((arch) => {
        if (!arch) return;
        macArch = arch;
        archConfirmed = true;
      });
    }
  });

  const featured = $derived(
    primary === 'macos-arm' || primary === 'macos-intel' ? BUILDS[macArch] : BUILDS[primary]
  );

  // Everything except whichever platform is featured, for the "other platforms" row.
  const others = $derived(
    (['macos-arm', 'macos-intel', 'windows', 'linux'] as PlatformId[]).filter((id) => {
      if (primary === 'macos-arm' || primary === 'macos-intel') {
        return id !== 'macos-arm' && id !== 'macos-intel';
      }
      return id !== primary;
    })
  );

  const highlights = [
    {
      title: 'A vault that unlocks with your face',
      body: 'Face ID, Touch ID, Windows Hello, or a passkey. One unlock, and every credential your AI world needs is available — and sealed again the moment you step away.'
    },
    {
      title: 'Runs on your machine',
      body: 'The gateway and your encrypted vault live on your own device. No third party holds your keys, because you are the third party.'
    },
    {
      title: 'Passwords, 2FA, and secrets in one place',
      body: 'Import from Bitwarden, Chrome, or 1Password. Generate codes for your 2FA. Give each AI entity its own scoped credentials.'
    },
    {
      title: 'Built to outlast your laptop',
      body: 'A recovery code and passphrase-encrypted backups mean the vault survives a lost device, a new machine, and ten years of change.'
    }
  ];

  const steps: Record<PlatformId, string[]> = {
    'macos-arm': [
      'Open the downloaded .dmg and drag Consistent CEO to Applications.',
      'First launch: right-click the app → Open, to approve it (it is signed but not yet notarised on every build).',
      'Create your vault, save the recovery code, and add Touch ID.'
    ],
    'macos-intel': [
      'Open the downloaded .dmg and drag Consistent CEO to Applications.',
      'First launch: right-click the app → Open, to approve it.',
      'Create your vault, save the recovery code, and add Touch ID.'
    ],
    windows: [
      'Run the downloaded setup .exe.',
      'If SmartScreen appears, choose “More info” → “Run anyway” (unsigned builds are flagged by default).',
      'Create your vault, save the recovery code, and add Windows Hello.'
    ],
    linux: [
      'Clone the repository and install the Rust toolchain and Node.',
      'Build the desktop shell: cd desktop && npm ci && npm run tauri build.',
      'Or run the gateway directly: cargo run -p consistent-world-gateway.'
    ]
  };
</script>

<Seo
  title="Download Consistent CEO"
  description="Download the Consistent CEO desktop app — your proactive AI operator with a built-in, passkey-unlocked password vault. For macOS and Windows."
/>

<div class="download">
  <section class="hero">
    <div class="badge"><VaultIcon size={16} /> Desktop app</div>
    <h1>Your AI world, on your machine.</h1>
    <p class="lede">
      Consistent CEO runs locally and carries a password vault you unlock with your face — logins,
      2FA, and the secrets every AI entity you run depends on, all sealed behind one passkey.
    </p>

    <div class="cta">
      {#if featured.url}
        <a class="btn primary big" href={featured.url} data-sveltekit-reload>
          {featured.label}
          <span class="chip">{featured.kind}</span>
        </a>
      {:else if featured.kind === 'source'}
        <!-- No prebuilt Linux desktop bundle yet: the README's from-source
             steps, or install.sh for the headless gateway. -->
        <a class="btn primary big" href={SOURCE_URL} target="_blank" rel="noopener">
          {featured.label}
        </a>
      {:else}
        <a class="btn primary big" href="/#early-access">
          Join early access
        </a>
      {/if}

      {#if primary === 'macos-arm' || primary === 'macos-intel'}
        <div class="arch">
          <button class:active={macArch === 'macos-arm'} onclick={() => (macArch = 'macos-arm')}>
            Apple Silicon
          </button>
          <button class:active={macArch === 'macos-intel'} onclick={() => (macArch = 'macos-intel')}>
            Intel
          </button>
        </div>
      {/if}
    </div>
    <p class="detail muted">
      {#if (primary === 'macos-arm' || primary === 'macos-intel') && !archConfirmed}
        Detected {osLabel}. Your browser won't say which chip — assuming
        <strong>{featured.detail}</strong>; switch above if that's wrong.
      {:else}
        Detected {osLabel} · {featured.detail}. Not right?
      {/if}
      <a href={RELEASES_URL} target="_blank" rel="noopener">See all downloads</a>.
    </p>
  </section>

  <section class="highlights">
    {#each highlights as h (h.title)}
      <div class="card">
        <h3>{h.title}</h3>
        <p class="muted">{h.body}</p>
      </div>
    {/each}
  </section>

  <section class="platforms">
    <h2>All platforms</h2>
    <div class="platform-row">
      {#each others as id (id)}
        {@const build = BUILDS[id]}
        <div class="platform">
          <div class="platform-meta">
            <strong>{build.detail}</strong>
            <span class="muted small">{build.kind === 'source' ? 'From source' : build.kind}</span>
          </div>
          {#if build.url}
            <a class="btn" href={build.url} data-sveltekit-reload>Download</a>
          {:else if build.kind === 'source'}
            <a class="btn" href={SOURCE_URL} target="_blank" rel="noopener">Instructions</a>
          {:else}
            <a class="btn" href="/#early-access">Join early access</a>
          {/if}
        </div>
      {/each}
    </div>
  </section>

  <section class="install">
    <h2>After you download</h2>
    <ol>
      {#each steps[primary === 'macos-intel' ? 'macos-intel' : primary] as step, i (i)}
        <li>{step}</li>
      {/each}
    </ol>
    <p class="muted small">
      New to it? The <a href="/vault">vault</a> walks you through creating your master password,
      saving your recovery code, and enrolling a passkey — in that order, so you are never one lost
      device away from being locked out.
    </p>
  </section>

  <section class="mobile-note card">
    <h3>On your phone</h3>
    <p class="muted">
      The mobile app (iOS &amp; Android) unlocks the same vault with Face ID or Touch ID and is
      distributed through the app stores. Point it at your gateway to sync.
    </p>
  </section>
</div>

<style>
  .download {
    max-width: 960px;
    margin: 0 auto;
    padding: var(--space-6, 32px) var(--space-4, 16px) var(--space-8, 64px);
    display: flex;
    flex-direction: column;
    gap: var(--space-8, 56px);
  }

  .hero {
    text-align: center;
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-3, 12px);
  }
  .badge {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    padding: 4px 12px;
    border: 1px solid var(--border);
    border-radius: 999px;
    color: var(--text-muted);
    font-size: var(--text-sm, 0.85rem);
  }
  h1 {
    font-size: clamp(2rem, 5vw, 3rem);
    line-height: 1.1;
    margin: 0;
    max-width: 16ch;
  }
  .lede {
    color: var(--text-muted);
    max-width: 54ch;
    font-size: 1.05rem;
    line-height: 1.5;
    margin: 0;
  }

  .cta {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: var(--space-2, 10px);
    margin-top: var(--space-3, 12px);
  }
  .btn {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    padding: 9px 16px;
    border-radius: var(--radius-md, 10px);
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font: inherit;
    text-decoration: none;
    cursor: pointer;
  }
  .btn:hover {
    border-color: var(--accent-orange, #c98a4b);
  }
  .btn.primary {
    background: var(--accent-orange, #c98a4b);
    border-color: var(--accent-orange, #c98a4b);
    color: #fff;
    font-weight: 600;
  }
  .btn.big {
    padding: 13px 28px;
    font-size: 1.05rem;
  }
  .chip {
    font-size: 0.75rem;
    opacity: 0.85;
    background: rgba(255, 255, 255, 0.2);
    padding: 1px 7px;
    border-radius: 999px;
  }
  .arch {
    display: inline-flex;
    gap: 4px;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 3px;
  }
  .arch button {
    background: transparent;
    border: none;
    border-radius: 999px;
    padding: 4px 14px;
    color: var(--text-muted);
    font: inherit;
    font-size: var(--text-sm, 0.85rem);
    cursor: pointer;
  }
  .arch button.active {
    background: var(--bg-selected, rgba(201, 138, 75, 0.15));
    color: var(--text-primary);
  }
  .detail a,
  .install a {
    color: var(--accent-orange, #c98a4b);
  }

  .highlights {
    display: grid;
    grid-template-columns: repeat(auto-fit, minmax(240px, 1fr));
    gap: var(--space-4, 16px);
  }
  .card {
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-lg, 14px);
    padding: var(--space-4, 20px);
  }
  .card h3 {
    margin: 0 0 8px;
    font-size: 1.05rem;
  }
  .card p {
    margin: 0;
    line-height: 1.5;
  }

  .platforms h2,
  .install h2 {
    font-size: 1.3rem;
    margin: 0 0 var(--space-3, 14px);
  }
  .platform-row {
    display: flex;
    flex-wrap: wrap;
    gap: var(--space-3, 12px);
  }
  .platform {
    flex: 1;
    min-width: 200px;
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: var(--space-3, 12px);
    padding: var(--space-3, 14px);
    border: 1px solid var(--border);
    border-radius: var(--radius-md, 10px);
    background: var(--bg-card);
  }
  .platform-meta {
    display: flex;
    flex-direction: column;
  }

  .install ol {
    line-height: 1.7;
    padding-left: 1.2em;
    color: var(--text-primary);
  }
  .muted {
    color: var(--text-muted);
  }
  .small {
    font-size: var(--text-sm, 0.85rem);
  }
</style>
