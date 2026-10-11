<script lang="ts">
  import { onDestroy, onMount } from 'svelte';
  import Seo from '$lib/components/Seo.svelte';
  import {
    GLOBAL_SCOPE,
    KIND_LABELS,
    UNLOCK_KIND_LABELS,
    VaultLockedError,
    acceptShare,
    auditLog,
    breachCheck,
    changeMasterPassword,
    copyToClipboard,
    createItem,
    createShare,
    deleteItem,
    deleteSecret,
    exportVault,
    generate,
    getItemSecret,
    getPublicKey,
    getSecret,
    getStatus,
    importCsv,
    importVault,
    initVault,
    itemScopes,
    itemTotp,
    listItems,
    listScopes,
    moveItem,
    listSecrets,
    lockVault,
    newRecoveryCode,
    passwordStrength,
    removeUnlockMethod,
    setSecret,
    unlockVault,
    unlockWithRecoveryCode,
    updateItemMetadata,
    updateItemSecret,
    vaultHealth,
    TooManyAttemptsError,
    VaultHttpError,
    type AuditEntry,
    type BreachResult,
    type HealthReport,
    type ImportOutcome,
    type ItemKind,
    type ItemScopeSummary,
    type ItemSecret,
    type ItemSummary,
    type PasswordStrength,
    type ScopeSummary,
    type TotpCode,
    type UnlockMethod,
    type VaultStatus
  } from '$lib/vault';
  import {
    PasskeyUnsupportedError,
    enrollPasskey,
    isPasskeySupported,
    passkeyMethods,
    unlockWithPasskey
  } from '$lib/vaultPasskey';
  import {
    canUnlockWithComputer,
    enrollThisComputer,
    isDesktopUnlockSupported,
    revokeThisComputer,
    unlockWithComputer
  } from '$lib/vaultDesktop';
  import { getTenantId, listEntities, type Entity } from '$lib/api';

  type Tab = 'passwords' | 'environment' | 'security';

  let status = $state<VaultStatus | null>(null);
  // When `status` was last confirmed by the gateway, so the page can tell when
  // the vault must have idle-locked even if it cannot reach the gateway to ask.
  let statusAt = 0;
  // The gateway answered 503: it runs without vault support at all.
  let vaultUnavailable = $state(false);
  // The last status call failed (401, network). The page keeps the last known
  // status and keeps polling rather than declaring the vault gone.
  let statusProblem = $state<string | null>(null);
  let polling = false;
  let loading = $state(true);
  let tab = $state<Tab>('passwords');
  let error = $state<string | null>(null);
  let notice = $state<string | null>(null);

  // Unlock / create
  let masterPassword = $state('');
  let confirmPassword = $state('');
  let busy = $state(false);

  // Live master-password strength at creation. Debounced so we do not score on
  // every keystroke; the zxcvbn call is on the server.
  let strength = $state<PasswordStrength | null>(null);
  let strengthTimer: ReturnType<typeof setTimeout> | null = null;

  // The current TOTP code for the open item: fetched once per period, at the
  // rollover, and counted down locally in between.
  let openTotp = $state<TotpCode | null>(null);
  let totpProblem = $state<string | null>(null);
  let totpFetchTimer: ReturnType<typeof setTimeout> | null = null;
  let totpCountdown: ReturnType<typeof setInterval> | null = null;
  // Bumped by stopTotp, so a late response or timer from an item that has since
  // closed (or been reopened) cannot revive a loop that was stopped.
  let totpRun = 0;

  // CSV import from another manager.
  let showImport = $state(false);
  let importText = $state('');
  let importPreview = $state<ImportOutcome | null>(null);

  // The workspace's entities, so every one of them gets a scope of its own in
  // both switchers without the user having to know — let alone type — its id.
  let entities = $state<Entity[]>([]);

  // Items. Each entity has its own password set; `itemScope` is which one the
  // Passwords tab is showing, mirroring the Environment tab's scope switcher.
  let items = $state<ItemSummary[]>([]);
  let itemScope = $state(GLOBAL_SCOPE);
  let itemScopeList = $state<ItemScopeSummary[]>([]);
  let search = $state('');
  let openItemId = $state<string | null>(null);
  let openSecret = $state<ItemSecret | null>(null);
  // The open item's secret as loaded or last saved, to tell unsaved edits apart.
  let openSecretSaved = '';
  let revealPassword = $state(false);
  let revealTotpSeed = $state(false);

  // Environment secrets
  let secretKeys = $state<string[]>([]);
  let revealedSecrets = $state<Record<string, string>>({});
  let newSecretKey = $state('');
  let newSecretValue = $state('');

  // Which scope the Environment tab is editing. `global` holds account-wide
  // defaults; an entity scope shadows them for that entity only.
  let scope = $state(GLOBAL_SCOPE);
  let scopes = $state<ScopeSummary[]>([]);

  // A recovery code is readable exactly once, at the moment it is minted. It is
  // held here until the user confirms they have written it down, and never
  // refetched — there is nothing to refetch it from.
  let recoveryCode = $state<string | null>(null);
  let recoveryAcknowledged = $state(false);

  // Unlocking with something other than the master password.
  let recoveryInput = $state('');
  let showRecoveryUnlock = $state(false);
  const passkeySupported = isPasskeySupported();
  const enrolledPasskeys = $derived(passkeyMethods(status?.unlockMethods ?? []));

  // Inside the desktop shell the vault unlocks with a key from the OS keychain
  // instead of a passkey — embedded webviews do not support PRF dependably.
  const desktopSupported = isDesktopUnlockSupported();
  let computerReady = $state(false);

  let newPasskeyLabel = $state('');
  let auditEntries = $state<AuditEntry[]>([]);
  let health = $state<HealthReport | null>(null);
  let breach = $state<BreachResult | null>(null);
  let breachRunning = $state(false);
  // Whether the open item's previous-password list is expanded.
  let showHistory = $state(false);

  let backupPassphrase = $state('');
  let restorePassphrase = $state('');
  let restoreJson = $state('');

  // Item composer
  let showComposer = $state(false);
  let draftKind = $state<ItemKind>('login');
  let draftName = $state('');
  let draftUsername = $state('');
  let draftPassword = $state('');
  let draftUri = $state('');
  let draftNotes = $state('');
  let draftFolder = $state('');
  let draftTotp = $state('');

  // Generator
  let showGenerator = $state(false);
  let genMode = $state<'password' | 'passphrase'>('password');
  let genLength = $state(20);
  let genSymbols = $state(true);
  let genDigits = $state(true);
  let genAvoidAmbiguous = $state(false);
  let genWords = $state(5);
  let genValue = $state('');

  // Sharing
  let showShare = $state(false);
  let shareItemId = $state<string | null>(null);
  let shareRecipientKey = $state('');
  let shareScenario = $state('');
  let shareExpiryHours = $state(24);
  let shareEnvelopeJson = $state('');
  let myPublicKey = $state('');
  let incomingEnvelope = $state('');

  // Master password rotation
  let showRotate = $state(false);
  let rotateCurrent = $state('');
  let rotateNext = $state('');

  let countdownTimer: ReturnType<typeof setInterval> | null = null;

  const filteredItems = $derived(
    search.trim()
      ? items.filter((i) => {
          const q = search.toLowerCase();
          return (
            i.name.toLowerCase().includes(q) ||
            i.uris.some((u) => u.toLowerCase().includes(q)) ||
            (i.folder ?? '').toLowerCase().includes(q)
          );
        })
      : items
  );

  /** Group the visible items by category (folder), uncategorised last, each group
   *  alphabetised — so a scope reads as a set of labelled drawers. */
  const groupedItems = $derived.by(() => {
    const groups = new Map<string, ItemSummary[]>();
    for (const item of filteredItems) {
      const key = item.folder?.trim() || '';
      (groups.get(key) ?? groups.set(key, []).get(key)!).push(item);
    }
    return [...groups.entries()]
      .sort(([a], [b]) => {
        if (a === '') return 1; // uncategorised sinks to the bottom
        if (b === '') return -1;
        return a.localeCompare(b);
      })
      .map(([folder, list]) => ({
        folder,
        // Favourites float to the top of their category, then alphabetical.
        items: list.sort(
          (x, y) => Number(y.favorite) - Number(x.favorite) || x.name.localeCompare(y.name)
        )
      }));
  });

  /** Scope ids are map keys inside the vault file, so the server restricts them
   *  to `[A-Za-z0-9._-]`. Entity ids are uuids and always pass; check anyway so
   *  an oddly-named entity is quietly skipped rather than making every list
   *  call for that scope fail. */
  const SCOPE_ID = /^[A-Za-z0-9._-]+$/;

  type ScopeChip = { scope: string; label: string; count: number };

  /** One scope per entity, listed whether or not it holds anything yet — that
   *  is what makes an entity's password set exist without anybody creating it.
   *  The auto-provisioned Personal space is left out: the global scope already
   *  *is* the CEO's own set, and two chips both reading "Personal" is worse
   *  than one. */
  const entityChips = $derived(
    entities
      .filter((e) => !e.isPersonal && SCOPE_ID.test(e.id))
      .map((e) => ({ scope: `entity:${e.id}`, label: e.name?.trim() || e.id }))
      .sort((a, b) => a.label.localeCompare(b.label))
  );

  /** The entity chips carrying this tab's counts, then any scope the vault
   *  already holds that no longer maps to a live entity — a deleted entity, or
   *  a `project:` scope set from the CLI. Those keep a chip so nothing stored
   *  becomes unreachable; the active scope is pinned in for the same reason. */
  function scopeChips(
    stored: { scope: string; label: string }[],
    countOf: Map<string, number>,
    active: string
  ): ScopeChip[] {
    const chips = new Map<string, ScopeChip>();
    for (const { scope: id, label } of entityChips) {
      chips.set(id, { scope: id, label, count: countOf.get(id) ?? 0 });
    }
    for (const row of [...stored, { scope: active, label: active }]) {
      if (row.scope === GLOBAL_SCOPE || chips.has(row.scope)) continue;
      chips.set(row.scope, {
        scope: row.scope,
        label: row.label,
        count: countOf.get(row.scope) ?? 0
      });
    }
    return [...chips.values()];
  }

  const itemScopeChips = $derived(
    scopeChips(itemScopeList, new Map(itemScopeList.map((s) => [s.scope, s.itemCount])), itemScope)
  );
  const envScopeChips = $derived(
    scopeChips(scopes, new Map(scopes.map((s) => [s.scope, s.keyCount])), scope)
  );

  /** Human name for a scope, for prose and dropdowns — never the raw uuid. */
  function scopeLabel(target: string, globalLabel = 'Personal'): string {
    if (target === GLOBAL_SCOPE) return globalLabel;
    return (
      entityChips.find((c) => c.scope === target)?.label ??
      target.replace(/^(entity|project):/, '')
    );
  }

  const lockCountdown = $derived.by(() => {
    const s = status?.locksInSeconds;
    if (s === undefined || s === null) return null;
    const m = Math.floor(s / 60);
    return m >= 1 ? `${m}m` : `${s}s`;
  });

  function flash(message: string) {
    notice = message;
    setTimeout(() => {
      if (notice === message) notice = null;
    }, 2500);
  }

  /// Any call can come back 423 if the idle timer fired between renders; when it
  /// does, drop straight back to the unlock screen instead of showing an error.
  async function guard<T>(fn: () => Promise<T>): Promise<T | null> {
    try {
      error = null;
      return await fn();
    } catch (e) {
      if (e instanceof VaultLockedError) {
        await settleLocked();
        return null;
      }
      if (e instanceof TooManyAttemptsError) {
        // Surface the wait plainly; the message already carries the seconds.
        error = e.message;
        return null;
      }
      error = e instanceof Error ? e.message : String(e);
      return null;
    }
  }

  /** Adopt a status the gateway just returned (from unlock, create, a poll). */
  function adoptStatus(next: VaultStatus) {
    status = next;
    statusAt = Date.now();
    statusProblem = null;
    vaultUnavailable = false;
  }

  /** Vault status alone — a plain gateway call, cheap to repeat. Returns whether
   *  the gateway answered. A failed call keeps the last known status instead of
   *  blanking the page: an expired session or a network blip is not "there is no
   *  vault here", and only a 503 means that. */
  async function refreshVaultStatus(): Promise<boolean> {
    try {
      adoptStatus(await getStatus());
      return true;
    } catch (e) {
      if (e instanceof VaultHttpError && e.status === 503) {
        status = null;
        statusProblem = null;
        vaultUnavailable = true;
      } else if (e instanceof VaultHttpError && e.status === 401) {
        statusProblem = 'Your session has expired. Sign in again to keep using the vault.';
      } else if (e instanceof VaultHttpError) {
        statusProblem = `The vault did not answer (${e.message}). Retrying…`;
      } else {
        statusProblem = 'Cannot reach the gateway right now. Retrying…';
      }
      return false;
    }
  }

  async function refreshStatus(): Promise<boolean> {
    const fresh = await refreshVaultStatus();
    // Only offer the desktop button if this machine is enrolled *and* the vault
    // still recognises that enrolment.
    //
    // Kept out of the polling path on purpose: this reaches the OS keychain,
    // and on desktop a keychain read the signature is not yet trusted for is a
    // password dialog. Enrolment only changes through the buttons on this page,
    // so recomputing it on a timer bought nothing and cost a prompt each time.
    if (fresh && status) computerReady = await canUnlockWithComputer(status.unlockMethods ?? []);
    else if (!status) computerReady = false;
    return fresh;
  }

  /** Whether the vault has idle-locked on the gateway by now, judging only from
   *  the last status it gave us. Activity since then only pushes the real
   *  deadline later, so this errs towards locking early — the safe direction. */
  function vaultMustHaveLocked(): boolean {
    const left = status?.locksInSeconds;
    return typeof left === 'number' && Date.now() - statusAt >= left * 1000;
  }

  /** Forget everything decrypted while the vault was open: item lists and
   *  secrets, revealed variables, reports, generated values, half-typed drafts
   *  and the CSV or passwords sitting in a form. Runs on an explicit lock, on any
   *  423, and when the status poll sees the vault lock — so plaintext never
   *  outlives the key on the gateway.
   *
   *  The recovery-code panel is the one thing left standing: that code is already
   *  a live way in and exists nowhere else, so wiping it before it is written
   *  down would strand a credential nobody has seen. */
  function wipeUnlockedState() {
    closeItem();
    items = [];
    itemScopeList = [];
    secretKeys = [];
    scopes = [];
    revealedSecrets = {};
    newSecretKey = '';
    newSecretValue = '';
    health = null;
    breach = null;
    auditEntries = [];
    showGenerator = false;
    genValue = '';
    showComposer = false;
    draftName = draftUsername = draftPassword = draftUri = draftNotes = '';
    draftFolder = draftTotp = '';
    showImport = false;
    importText = '';
    importPreview = null;
    showShare = false;
    shareItemId = null;
    shareRecipientKey = '';
    shareScenario = '';
    shareEnvelopeJson = '';
    incomingEnvelope = '';
    myPublicKey = '';
    showRotate = false;
    rotateCurrent = '';
    rotateNext = '';
    backupPassphrase = '';
    restorePassphrase = '';
    restoreJson = '';
  }

  /** The vault is locked — we just locked it, or the gateway answered 423. Wipe
   *  the page and show the unlock prompt. */
  async function settleLocked() {
    wipeUnlockedState();
    const fresh = await refreshStatus();
    if (fresh) {
      // Unlocked again in the meantime (from another device): show it, filled.
      if (status?.unlocked) await loadAll();
    } else if (status?.unlocked) {
      // The status call failed, but the lock is a fact: do not let the last
      // known "unlocked" bring back an empty unlocked view.
      status = { ...status, unlocked: false, locksInSeconds: undefined };
    }
  }

  /** Every 20s: keep the auto-lock countdown honest, notice when the gateway
   *  locks the vault, and recover from a status call that failed. */
  async function pollStatus() {
    if (polling || vaultUnavailable) return;
    // A known-locked (or not yet created) vault has no countdown to keep.
    if (status && !status.unlocked && !statusProblem) return;
    polling = true;
    try {
      const wasUnlocked = status?.unlocked === true;
      if (!status) {
        // The first load failed; this is the retry.
        if (await refreshStatus()) await loadAll();
        return;
      }
      if (await refreshVaultStatus()) {
        if (wasUnlocked && !status?.unlocked) wipeUnlockedState();
        else if (!wasUnlocked && status?.unlocked) await loadAll();
      } else if (wasUnlocked && vaultMustHaveLocked()) {
        // Out of touch for longer than the idle window: the gateway has locked
        // the vault by now, so what it decrypted must leave this page too.
        wipeUnlockedState();
        if (status) status = { ...status, unlocked: false, locksInSeconds: undefined };
      }
    } finally {
      polling = false;
    }
  }

  /** Entities come from the workspace, not the vault, so this works while the
   *  vault is still locked and does not need `guard`. A failure just means the
   *  switcher falls back to whatever scopes the vault itself reports. */
  async function loadEntities() {
    try {
      const res = await listEntities({ limit: 200 });
      entities = res.items ?? [];
    } catch (_) {
      // Non-fatal; the vault is still fully usable without the entity names.
    }
  }

  async function loadAll() {
    // Ahead of the lock check: entities are not vault data, and refreshing them
    // on every reload is what makes an entity created elsewhere show up here.
    void loadEntities();
    if (!status?.unlocked) return;
    const [loadedItems, loadedItemScopes, loadedKeys, loadedScopes] = await Promise.all([
      guard(() => listItems(itemScope)),
      guard(itemScopes),
      guard(() => listSecrets(scope)),
      guard(listScopes)
    ]);
    // A sibling call may have hit 423 and wiped the page; do not refill it.
    if (!status?.unlocked) return;
    if (loadedItems) items = loadedItems;
    if (loadedItemScopes) itemScopeList = loadedItemScopes;
    if (loadedKeys) secretKeys = loadedKeys;
    if (loadedScopes) scopes = loadedScopes;
  }

  /** Comparable form of an item's secret half. `hidden` is left out: masking a
   *  custom field to peek at it is not an edit worth a "discard?" prompt. */
  const fingerprint = (secret: ItemSecret) =>
    JSON.stringify(secret, (key, value) => (key === 'hidden' ? undefined : value));

  /** Ask before something throws away unsaved edits to the open item. */
  function confirmDiscard(): boolean {
    if (!openSecret || fingerprint(openSecret) === openSecretSaved) return true;
    return confirm('Discard your unsaved changes to this item?');
  }

  /** Reload the current set without closing the open item, which keeps any
   *  unsaved edits — unless the item is no longer in this set (moved, deleted). */
  async function reloadItems() {
    const [loaded, loadedScopes] = await Promise.all([
      guard(() => listItems(itemScope)),
      guard(itemScopes)
    ]);
    if (!status?.unlocked) return;
    if (loaded) {
      items = loaded;
      if (openItemId && !loaded.some((i) => i.id === openItemId)) closeItem();
    }
    if (loadedScopes) itemScopeList = loadedScopes;
  }

  /** Switch which entity's password set the Passwords tab shows. The open item
   *  belongs to the old set, so it closes — after asking, if it has unsaved
   *  edits. Returns false if the user chose to stay. */
  async function switchItemScope(next: string): Promise<boolean> {
    if (!confirmDiscard()) return false;
    itemScope = next;
    closeItem();
    const [loaded, loadedScopes] = await Promise.all([
      guard(() => listItems(next)),
      guard(itemScopes)
    ]);
    if (!status?.unlocked) return true;
    items = loaded ?? [];
    if (loadedScopes) itemScopeList = loadedScopes;
    return true;
  }

  /** Moving an item out of this set closes it, so ask first if that would drop
   *  unsaved edits; if the move fails, the item stays open with its edits. */
  async function moveItemToScope(item: ItemSummary, picker: HTMLSelectElement) {
    const targetScope = picker.value;
    if (!confirmDiscard()) {
      picker.value = item.scope;
      return;
    }
    const ok = await guard(() => moveItem(item.id, targetScope));
    if (ok === null) {
      picker.value = item.scope;
      return;
    }
    await reloadItems();
    flash(`Moved to ${scopeLabel(targetScope)}.`);
  }

  /** Switching scope reloads only the variable list; items are scope-independent. */
  async function switchScope(next: string) {
    scope = next;
    revealedSecrets = {};
    const keys = await guard(() => listSecrets(next));
    secretKeys = keys ?? [];
  }

  /** Score the candidate master password, debounced, with the user as context. */
  function onMasterPasswordInput() {
    if (strengthTimer) clearTimeout(strengthTimer);
    const candidate = masterPassword;
    if (candidate.length < 4) {
      strength = null;
      return;
    }
    strengthTimer = setTimeout(async () => {
      // The user's own workspace counts as weak-for-them context: a password
      // built from it is guessable by anyone who knows them.
      const context = [getTenantId(), 'consistent', 'vault'].filter(Boolean);
      strength = await passwordStrength(candidate, context).catch(() => null);
    }, 250);
  }

  async function create() {
    if (busy) return;
    if (masterPassword !== confirmPassword) {
      error = 'Passwords do not match';
      return;
    }
    if (masterPassword.length < 8) {
      error = 'Use at least 8 characters for your master password';
      return;
    }
    // Weak is a warning, not a wall — but a very weak master password is worth a
    // deliberate confirmation, since nothing sits behind it.
    if (strength && strength.score <= 1) {
      const proceed = confirm(
        `That master password is ${strength.label.toLowerCase()}. It protects everything in your vault and cannot be reset if forgotten. Use it anyway?`
      );
      if (!proceed) return;
    }
    busy = true;
    const next = await guard(() => initVault(masterPassword));
    busy = false;
    if (next) {
      // Capture this before anything else can navigate away: the code exists
      // nowhere else, in this response or never. It is kept out of `status`, so
      // the recovery panel's own state is the only place it lives.
      const { recoveryCode: code, ...created } = next;
      recoveryCode = code;
      recoveryAcknowledged = false;
      adoptStatus(created);
      masterPassword = '';
      confirmPassword = '';
      await loadAll();
    }
  }

  // Every unlock path returns early while another attempt is in flight. The
  // buttons are disabled, but Enter in the inputs is not, and each parallel
  // attempt is a slow Argon2id guess that counts against the unlock throttle.
  async function unlock() {
    if (busy) return;
    busy = true;
    const next = await guard(() => unlockVault(masterPassword));
    busy = false;
    if (next) {
      adoptStatus(next);
      masterPassword = '';
      await loadAll();
    }
  }

  async function unlockByPasskey() {
    if (busy) return;
    busy = true;
    try {
      adoptStatus(await unlockWithPasskey(status?.unlockMethods ?? []));
      await loadAll();
    } catch (e) {
      // A cancelled Face ID prompt is a normal thing to do, not an error worth
      // shouting about.
      const message = e instanceof Error ? e.message : String(e);
      if (!/cancel/i.test(message)) error = message;
    } finally {
      busy = false;
    }
  }

  async function unlockByComputer() {
    if (busy) return;
    busy = true;
    try {
      adoptStatus(await unlockWithComputer());
      await loadAll();
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
      await refreshStatus();
    } finally {
      busy = false;
    }
  }

  async function addThisComputer() {
    busy = true;
    try {
      await enrollThisComputer('This computer');
      await refreshStatus();
      flash('This computer can now open your vault.');
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      busy = false;
    }
  }

  async function removeThisComputer() {
    if (!confirm('Remove this computer? It will no longer open your vault.')) return;
    const ok = await guard(() => revokeThisComputer());
    if (ok !== null) {
      await refreshStatus();
      flash('This computer removed.');
    }
  }

  async function unlockByRecovery() {
    if (busy) return;
    busy = true;
    const next = await guard(() => unlockWithRecoveryCode(recoveryInput));
    busy = false;
    if (next) {
      adoptStatus(next);
      recoveryInput = '';
      showRecoveryUnlock = false;
      await loadAll();
      flash('Unlocked with your recovery code. Consider enrolling a passkey.');
    }
  }

  // ---- security ---------------------------------------------------------------

  async function addPasskey() {
    const label = newPasskeyLabel.trim() || 'Passkey';
    busy = true;
    try {
      await enrollPasskey(label, { id: 'vault', name: label });
      newPasskeyLabel = '';
      await refreshStatus();
      flash(`${label} can now open your vault.`);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (e instanceof PasskeyUnsupportedError) error = message;
      else if (!/cancel/i.test(message)) error = message;
    } finally {
      busy = false;
    }
  }

  async function revokeMethod(method: UnlockMethod) {
    const label = UNLOCK_KIND_LABELS[method.kind];
    if (!confirm(`Remove "${method.label}" (${label})? It will no longer open your vault.`)) {
      return;
    }
    const ok = await guard(() => removeUnlockMethod(method.id));
    if (ok !== null) {
      await refreshStatus();
      flash(`${method.label} removed.`);
    }
  }

  async function mintRecoveryCode() {
    // A double click would mint two codes and show only the second, leaving a
    // working way into the vault that nobody ever saw.
    if (busy) return;
    busy = true;
    try {
      const code = await guard(newRecoveryCode);
      if (code) {
        recoveryCode = code;
        recoveryAcknowledged = false;
        await refreshStatus();
      }
    } finally {
      busy = false;
    }
  }

  async function loadAudit() {
    const entries = await guard(auditLog);
    if (entries) auditEntries = entries;
  }

  async function loadHealth() {
    const report = await guard(vaultHealth);
    if (report) health = report;
  }

  async function runBreachCheck() {
    breachRunning = true;
    const result = await guard(breachCheck);
    breachRunning = false;
    if (result) {
      breach = result;
      if (result.breached.length === 0 && !result.unreachable) {
        flash(`Checked ${result.checked} passwords — none found in known breaches.`);
      }
    }
  }

  /** Restore a previous password from an item's history back into the editor. */
  function restorePassword(password: string) {
    if (openSecret) {
      openSecret = { ...openSecret, password };
      revealPassword = true;
      flash('Previous password loaded — Save to keep it.');
    }
  }

  /** Jump to an item flagged in the health report: switch to its entity's set,
   *  open it, and land on the Passwords tab. */
  async function goToItem(flag: { id: string; scope: string }) {
    tab = 'passwords';
    if (flag.scope !== itemScope && !(await switchItemScope(flag.scope))) return;
    // openItem toggles, so only call it when this item is not already open.
    if (openItemId !== flag.id) await openItem(flag.id);
  }

  /** Download the backup rather than showing it: it is a file to store, and
   *  putting it on screen invites a screenshot of everything you own. */
  async function downloadBackup() {
    if (backupPassphrase.length < 8) {
      error = 'Use at least 8 characters for the backup passphrase';
      return;
    }
    busy = true;
    const backup = await guard(() => exportVault(backupPassphrase));
    busy = false;
    if (!backup) return;

    const blob = new Blob([JSON.stringify(backup, null, 2)], { type: 'application/json' });
    const url = URL.createObjectURL(blob);
    const a = document.createElement('a');
    a.href = url;
    a.download = `consistent-vault-backup-${new Date().toISOString().slice(0, 10)}.json`;
    a.click();
    URL.revokeObjectURL(url);

    backupPassphrase = '';
    flash('Backup downloaded. Store it somewhere durable — it opens with its passphrase alone.');
  }

  async function restoreBackup() {
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(restoreJson);
    } catch {
      error = 'That does not look like a backup file';
      return;
    }
    busy = true;
    const result = await guard(() => importVault(parsed, restorePassphrase));
    busy = false;
    if (result) {
      restoreJson = '';
      restorePassphrase = '';
      await loadAll();
      flash(
        `Restored ${result.itemsAdded} item(s) and ${result.variablesAdded} variable(s). Nothing existing was overwritten.`
      );
    }
  }

  function onBackupFile(event: Event) {
    const file = (event.target as HTMLInputElement).files?.[0];
    if (!file) return;
    file.text().then((text) => (restoreJson = text));
  }

  async function lock() {
    try {
      await lockVault();
    } catch (e) {
      // The gateway did not confirm the lock, so the vault may still be open
      // there. Say so, and keep the page matching what the gateway reports
      // rather than showing a lock screen over a vault that is still unlocked.
      error = `Could not lock the vault: ${e instanceof Error ? e.message : String(e)}`;
      await refreshStatus();
      if (!status?.unlocked) wipeUnlockedState();
      return;
    }
    await settleLocked();
  }

  async function openItem(id: string) {
    if (!confirmDiscard()) return;
    if (openItemId === id) {
      closeItem();
      return;
    }
    const secret = await guard(() => getItemSecret(id));
    if (secret && status?.unlocked) {
      openItemId = id;
      openSecret = secret;
      openSecretSaved = fingerprint(secret);
      revealPassword = false;
      revealTotpSeed = false;
      showHistory = false;
      startTotp(id, secret);
    }
  }

  function closeItem() {
    openItemId = null;
    openSecret = null;
    openSecretSaved = '';
    revealPassword = false;
    revealTotpSeed = false;
    showHistory = false;
    stopTotp();
  }

  /** Show the live TOTP code for an item with a 2FA seed: one request per code
   *  period, timed to the rollover, with the countdown run locally in between.
   *  Reading a code does not count as vault activity on the gateway, so an open
   *  item cannot hold the vault unlocked. A 423 hands over to the lock screen and
   *  a 400 (no usable seed) stops — asking again fixes neither. */
  function startTotp(id: string, secret: ItemSecret) {
    stopTotp();
    if (!secret.totpSecret?.trim()) return;
    const run = ++totpRun;
    let expiresAt = 0;

    const fetchCode = async () => {
      totpFetchTimer = null;
      try {
        const code = await itemTotp(id);
        if (run !== totpRun) return;
        openTotp = code;
        totpProblem = null;
        expiresAt = Date.now() + code.secondsRemaining * 1000;
        // A beat past the rollover, so the gateway is already on the next step.
        totpFetchTimer = setTimeout(fetchCode, code.secondsRemaining * 1000 + 250);
      } catch (e) {
        if (run !== totpRun) return;
        if (e instanceof VaultLockedError) {
          stopTotp();
          await settleLocked();
        } else if (e instanceof VaultHttpError && e.status === 400) {
          // Saving a corrected seed restarts this.
          stopTotp();
          totpProblem = `No code: ${e.message}`;
        } else {
          // A blip. Retry, but not every second.
          openTotp = null;
          totpProblem = 'Could not fetch the current code. Retrying…';
          totpFetchTimer = setTimeout(fetchCode, 10_000);
        }
      }
    };

    totpCountdown = setInterval(() => {
      if (run !== totpRun || !openTotp) return;
      const left = Math.max(0, Math.ceil((expiresAt - Date.now()) / 1000));
      if (left !== openTotp.secondsRemaining) openTotp = { ...openTotp, secondsRemaining: left };
    }, 1000);
    void fetchCode();
  }

  function stopTotp() {
    totpRun++;
    if (totpFetchTimer) {
      clearTimeout(totpFetchTimer);
      totpFetchTimer = null;
    }
    if (totpCountdown) {
      clearInterval(totpCountdown);
      totpCountdown = null;
    }
    openTotp = null;
    totpProblem = null;
  }

  // ---- import from another manager --------------------------------------------

  async function previewImport() {
    if (!importText.trim()) return;
    busy = true;
    // Dry run: parse and report, store nothing.
    importPreview = await guard(() => importCsv(importText, false));
    busy = false;
  }

  async function commitImport() {
    busy = true;
    // Import into whichever entity's set is currently open.
    const result = await guard(() => importCsv(importText, true, itemScope));
    busy = false;
    if (result) {
      importText = '';
      importPreview = null;
      showImport = false;
      await loadAll();
      flash(
        `Imported ${result.imported} login(s) from ${result.format}.` +
          (result.skipped.length ? ` ${result.skipped.length} row(s) skipped.` : '')
      );
    }
  }

  function onImportFile(event: Event) {
    const file = (event.target as HTMLInputElement).files?.[0];
    if (!file) return;
    file.text().then((text) => {
      importText = text;
      importPreview = null;
    });
  }

  /** `sensitive` copies (passwords, TOTP codes, recovery material) auto-clear
   *  from the clipboard after 30s. */
  async function copy(text: string | null | undefined, label: string, sensitive = false) {
    if (!text) return;
    const ok = await copyToClipboard(text, sensitive);
    flash(ok ? `${label} copied${sensitive ? ' — clears in 30s' : ''}` : 'Could not copy');
  }

  async function saveItem() {
    if (!draftName.trim()) {
      error = 'Give the item a name';
      return;
    }
    busy = true;
    const id = await guard(() =>
      createItem({
        kind: draftKind,
        name: draftName.trim(),
        // New items land in whichever entity's set is currently open.
        scope: itemScope,
        folder: draftFolder.trim() || null,
        uris: draftUri.trim() ? [draftUri.trim()] : [],
        secret: {
          username: draftUsername || null,
          password: draftPassword || null,
          totpSecret: draftTotp.trim() || null,
          notes: draftNotes || null
        }
      })
    );
    busy = false;
    if (id) {
      showComposer = false;
      draftName = draftUsername = draftPassword = draftUri = draftNotes = '';
      draftFolder = draftTotp = '';
      draftKind = 'login';
      await loadAll();
      flash('Item saved');
    }
  }

  async function saveOpenItem() {
    if (!openItemId || !openSecret) return;
    const id = openItemId;
    const saving = $state.snapshot(openSecret) as ItemSecret;
    busy = true;
    const ok = await guard(() => updateItemSecret(id, saving));
    busy = false;
    if (ok !== null) {
      if (openItemId === id) {
        // Edits typed while the save was in flight still count as unsaved.
        openSecretSaved = fingerprint(saving);
        // The code is read from the stored seed, which may just have changed.
        startTotp(id, saving);
      }
      await loadAll();
      flash('Item updated');
    }
  }

  // ---- custom fields (name/value pairs on an item's secret half) --------------

  function addCustomField() {
    if (!openSecret) return;
    openSecret = {
      ...openSecret,
      customFields: [...(openSecret.customFields ?? []), { name: '', value: '', hidden: false }]
    };
  }

  function removeCustomField(index: number) {
    if (!openSecret) return;
    openSecret = {
      ...openSecret,
      customFields: (openSecret.customFields ?? []).filter((_, i) => i !== index)
    };
  }

  /** Toggle an item's favourite flag and persist it immediately. */
  async function toggleFavorite(item: ItemSummary) {
    const ok = await guard(() => updateItemMetadata(item.id, { favorite: !item.favorite }));
    // A reload, not a scope switch: starring one item must not close another.
    if (ok !== null) await reloadItems();
  }

  async function removeItem(id: string, name: string) {
    if (!confirm(`Delete "${name}"? This cannot be undone.`)) return;
    const ok = await guard(() => deleteItem(id));
    if (ok !== null) {
      closeItem();
      await loadAll();
      flash('Item deleted');
    }
  }

  async function runGenerator() {
    const value = await guard(() =>
      generate(
        genMode === 'passphrase'
          ? { mode: 'passphrase', words: genWords, separator: '-', capitalize: true }
          : {
              mode: 'password',
              length: genLength,
              symbols: genSymbols,
              digits: genDigits,
              avoidAmbiguous: genAvoidAmbiguous
            }
      )
    );
    if (value) genValue = value;
  }

  function useGenerated() {
    if (!genValue) return;
    if (showComposer) draftPassword = genValue;
    else if (openSecret) openSecret = { ...openSecret, password: genValue };
    showGenerator = false;
    flash('Generated password applied');
  }

  async function revealSecret(key: string) {
    if (revealedSecrets[key] !== undefined) {
      const { [key]: _removed, ...rest } = revealedSecrets;
      revealedSecrets = rest;
      return;
    }
    const value = await guard(() => getSecret(key, scope));
    if (value !== null && status?.unlocked) revealedSecrets = { ...revealedSecrets, [key]: value };
  }

  async function saveSecret() {
    if (!newSecretKey.trim()) {
      error = 'Key is required';
      return;
    }
    busy = true;
    const ok = await guard(() => setSecret(newSecretKey.trim(), newSecretValue, scope));
    busy = false;
    if (ok !== null) {
      newSecretKey = '';
      newSecretValue = '';
      await loadAll();
      flash('Saved');
    }
  }

  async function removeSecret(key: string) {
    if (!confirm(`Delete ${key}?`)) return;
    const ok = await guard(() => deleteSecret(key, scope));
    if (ok !== null) {
      await loadAll();
      flash('Deleted');
    }
  }

  async function openShare(id: string) {
    shareItemId = id;
    shareEnvelopeJson = '';
    shareRecipientKey = '';
    shareScenario = '';
    showShare = true;
    const key = await guard(getPublicKey);
    if (key) myPublicKey = key;
  }

  async function buildShare() {
    if (!shareItemId || !shareRecipientKey.trim()) {
      error = "Paste the recipient's vault key";
      return;
    }
    busy = true;
    const expiresAt =
      shareExpiryHours > 0
        ? new Date(Date.now() + shareExpiryHours * 3600_000).toISOString()
        : undefined;
    const envelope = await guard(() =>
      createShare({
        itemId: shareItemId!,
        recipientPublicKey: shareRecipientKey.trim(),
        scenario: shareScenario.trim() || undefined,
        expiresAt
      })
    );
    busy = false;
    if (envelope) shareEnvelopeJson = JSON.stringify(envelope, null, 2);
  }

  async function receiveShare() {
    let parsed: Record<string, unknown>;
    try {
      parsed = JSON.parse(incomingEnvelope);
    } catch {
      error = 'That is not valid JSON';
      return;
    }
    busy = true;
    const id = await guard(() => acceptShare(parsed));
    busy = false;
    if (id) {
      incomingEnvelope = '';
      await loadAll();
      flash('Shared item added to your vault');
    }
  }

  async function rotateMaster() {
    if (rotateNext.length < 8) {
      error = 'Use at least 8 characters';
      return;
    }
    busy = true;
    const ok = await guard(() => changeMasterPassword(rotateCurrent, rotateNext));
    busy = false;
    if (ok !== null) {
      rotateCurrent = '';
      rotateNext = '';
      showRotate = false;
      flash('Master password changed');
    }
  }

  onMount(async () => {
    await refreshStatus();
    await loadAll();
    loading = false;
    countdownTimer = setInterval(pollStatus, 20_000);
  });

  onDestroy(() => {
    if (countdownTimer) clearInterval(countdownTimer);
    stopTotp();
    if (strengthTimer) clearTimeout(strengthTimer);
  });
</script>

<Seo title="Vault" noindex />

<div class="vault">
  <header class="head">
    <div>
      <h1>Vault</h1>
      <p class="sub">
        Passwords, API keys, and environment variables — encrypted with one master password
        that never leaves this machine.
      </p>
    </div>
    {#if status?.unlocked}
      <div class="head-actions">
        {#if lockCountdown}
          <span class="countdown" title="Auto-locks after inactivity">locks in {lockCountdown}</span>
        {/if}
        <button class="btn" onclick={() => (showRotate = !showRotate)}>Master password</button>
        <button class="btn" onclick={lock}>Lock</button>
      </div>
    {/if}
  </header>

  {#if error}
    <div class="banner error" role="alert">{error}</div>
  {/if}
  {#if statusProblem && status}
    <div class="banner error" role="status">{statusProblem}</div>
  {/if}
  {#if notice}
    <div class="banner ok" role="status">{notice}</div>
  {/if}

  {#if recoveryCode}
    <!-- Shown once. There is no server-side copy to fetch it back from, which is
         the security property and also the reason this is deliberately hard to
         dismiss by accident. -->
    <div class="panel recovery" role="alertdialog" aria-labelledby="recovery-title">
      <h2 id="recovery-title">Write this down now</h2>
      <p class="muted">
        This recovery code opens your vault if you lose your password and every device. It is
        shown once and is not stored anywhere we can retrieve it.
      </p>
      <code class="recovery-code">{recoveryCode}</code>
      <div class="row">
        <button class="btn" onclick={() => copy(recoveryCode, 'Recovery code', true)}>Copy</button>
        <button
          class="btn"
          onclick={() => {
            const blob = new Blob([recoveryCode ?? ''], { type: 'text/plain' });
            const url = URL.createObjectURL(blob);
            const a = document.createElement('a');
            a.href = url;
            a.download = 'consistent-vault-recovery-code.txt';
            a.click();
            URL.revokeObjectURL(url);
          }}
        >
          Download
        </button>
      </div>
      <label class="ack">
        <input type="checkbox" bind:checked={recoveryAcknowledged} />
        I have saved this somewhere safe and offline
      </label>
      <button
        class="btn primary"
        disabled={!recoveryAcknowledged}
        onclick={() => {
          recoveryCode = null;
          recoveryAcknowledged = false;
        }}
      >
        Done
      </button>
    </div>
  {/if}

  {#if loading}
    <p class="muted">Loading…</p>
  {:else if !status}
    <div class="panel">
      {#if statusProblem && !vaultUnavailable}
        <p class="muted">{statusProblem}</p>
      {:else}
        <p class="muted">The vault service is not available on this gateway.</p>
      {/if}
    </div>
  {:else if !status.initialized}
    <div class="panel gate">
      <h2>Create your vault</h2>
      <p class="muted">
        Pick a strong master password. Everything inside is encrypted with a key derived from
        it, and the gateway only holds that key while the vault is unlocked.
      </p>
      <p class="muted">
        You will also get a recovery code, shown once. Add a passkey afterwards and the
        password becomes the thing you rarely need.
      </p>
      <input
        type="password"
        placeholder="Master password"
        bind:value={masterPassword}
        oninput={onMasterPasswordInput}
      />

      {#if strength}
        <div class="strength" data-score={strength.score}>
          <div class="strength-bars" aria-hidden="true">
            {#each [0, 1, 2, 3] as i (i)}
              <span class:filled={i < strength.score}></span>
            {/each}
          </div>
          <div class="strength-text">
            <strong>{strength.label}</strong>
            <span class="muted small">~{strength.guessesLog10} guesses to crack</span>
          </div>
          {#if strength.warning}
            <p class="muted small">{strength.warning}</p>
          {:else if strength.suggestion && strength.score < 3}
            <p class="muted small">Tip: {strength.suggestion}</p>
          {/if}
        </div>
      {/if}

      <input type="password" placeholder="Confirm master password" bind:value={confirmPassword} />
      <button class="btn primary" onclick={create} disabled={busy}>
        {busy ? 'Creating…' : 'Create vault'}
      </button>
    </div>
  {:else if !status.unlocked}
    <div class="panel gate">
      <h2>Vault locked</h2>

      {#if computerReady}
        <button class="btn primary big" onclick={unlockByComputer} disabled={busy}>
          {busy ? 'Unlocking…' : 'Unlock with this computer'}
        </button>
        <p class="muted small">Using the key held in this machine's keychain.</p>
        <div class="divider"><span>or</span></div>
      {:else if enrolledPasskeys.length > 0 && passkeySupported}
        <button class="btn primary big" onclick={unlockByPasskey} disabled={busy}>
          {busy ? 'Waiting for your passkey…' : 'Unlock with passkey'}
        </button>
        <p class="muted small">Face ID, Touch ID, Windows Hello or a security key.</p>
        <div class="divider"><span>or</span></div>
      {/if}

      <input
        type="password"
        placeholder="Master password"
        bind:value={masterPassword}
        onkeydown={(e) => e.key === 'Enter' && unlock()}
      />
      <button class="btn" onclick={unlock} disabled={busy}>
        {busy ? 'Unlocking…' : 'Unlock with password'}
      </button>

      {#if showRecoveryUnlock}
        <div class="recovery-unlock">
          <p class="muted small">
            Enter your recovery code. Dashes, spaces and capitalisation do not matter.
          </p>
          <input
            placeholder="XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX-XXXX"
            bind:value={recoveryInput}
            onkeydown={(e) => e.key === 'Enter' && unlockByRecovery()}
          />
          <div class="row">
            <button class="btn primary" onclick={unlockByRecovery} disabled={busy}>
              Unlock with recovery code
            </button>
            <button class="btn" onclick={() => (showRecoveryUnlock = false)}>Cancel</button>
          </div>
        </div>
      {:else}
        <button class="link-btn" onclick={() => (showRecoveryUnlock = true)}>
          Lost your password and devices? Use a recovery code
        </button>
      {/if}
    </div>
  {:else}
    {#if showRotate}
      <div class="panel">
        <h2>Change master password</h2>
        <div class="row">
          <input type="password" placeholder="Current" bind:value={rotateCurrent} />
          <input type="password" placeholder="New" bind:value={rotateNext} />
          <button class="btn primary" onclick={rotateMaster} disabled={busy}>Change</button>
          <button class="btn" onclick={() => (showRotate = false)}>Cancel</button>
        </div>
        <p class="muted small">
          Your items are not re-encrypted — only the key that protects them is rewrapped, so this
          is instant regardless of vault size.
        </p>
      </div>
    {/if}

    <nav class="tabs">
      <button class:active={tab === 'passwords'} onclick={() => (tab = 'passwords')}>
        Passwords <span class="count">{items.length}</span>
      </button>
      <button class:active={tab === 'environment'} onclick={() => (tab = 'environment')}>
        Environment <span class="count">{secretKeys.length}</span>
      </button>
      <button
        class:active={tab === 'security'}
        onclick={() => {
          tab = 'security';
          loadAudit();
          loadHealth();
        }}
      >
        Security <span class="count">{status.unlockMethods.length}</span>
      </button>
    </nav>

    {#if tab === 'passwords'}
      <!-- Each entity has its own password set, listed from the workspace's own
           entities so there is nothing to create or type. This switcher mirrors
           the Environment tab's, so passwords and env vars organise the same way. -->
      <div class="scope-bar">
        <button
          class:active={itemScope === GLOBAL_SCOPE}
          onclick={() => switchItemScope(GLOBAL_SCOPE)}
        >
          Personal
        </button>
        {#each itemScopeChips as s (s.scope)}
          <button class:active={itemScope === s.scope} onclick={() => switchItemScope(s.scope)}>
            {s.label} <span class="count">{s.count}</span>
          </button>
        {/each}
        {#if entityChips.length === 0}
          <span class="muted small">No entities yet — everything lives in Personal.</span>
        {/if}
      </div>

      <div class="toolbar">
        <input class="search" placeholder="Search items…" bind:value={search} />
        <button class="btn" onclick={() => (showGenerator = !showGenerator)}>Generator</button>
        <button
          class="btn"
          onclick={() => {
            showImport = !showImport;
            importPreview = null;
          }}
        >
          {showImport ? 'Cancel' : 'Import'}
        </button>
        <button class="btn primary" onclick={() => (showComposer = !showComposer)}>
          {showComposer ? 'Cancel' : 'New item'}
        </button>
      </div>

      {#if showImport}
        <div class="panel">
          <h2>Import from another manager</h2>
          <p class="muted small">
            Export a CSV from Bitwarden, Chrome, 1Password, LastPass or Safari and drop it here.
            Nothing is stored until you review what was found. TOTP seeds come across too.
            Imports into <strong>{scopeLabel(itemScope)}</strong>.
          </p>
          <div class="row wrap">
            <input type="file" accept=".csv,text/csv" onchange={onImportFile} />
            <button class="btn" onclick={previewImport} disabled={busy || !importText.trim()}>
              Preview
            </button>
          </div>
          <textarea
            class="mono"
            rows="4"
            placeholder="…or paste CSV contents here"
            bind:value={importText}
            oninput={() => (importPreview = null)}
          ></textarea>

          {#if importPreview}
            <div class="import-preview">
              <p>
                <strong>{importPreview.detected}</strong> login(s) found — detected format:
                <strong>{importPreview.format}</strong>.
                {#if importPreview.skipped.length}
                  <span class="muted">{importPreview.skipped.length} row(s) will be skipped.</span>
                {/if}
              </p>
              {#if importPreview.skipped.length}
                <details>
                  <summary class="muted small">Show skipped rows</summary>
                  <ul class="muted small">
                    {#each importPreview.skipped.slice(0, 20) as reason (reason)}
                      <li>{reason}</li>
                    {/each}
                  </ul>
                </details>
              {/if}
              <button
                class="btn primary"
                onclick={commitImport}
                disabled={busy || importPreview.detected === 0}
              >
                Import {importPreview.detected} login(s)
              </button>
            </div>
          {/if}
        </div>
      {/if}

      {#if showGenerator}
        <div class="panel">
          <div class="row wrap">
            <select bind:value={genMode}>
              <option value="password">Password</option>
              <option value="passphrase">Passphrase</option>
            </select>
            {#if genMode === 'password'}
              <label class="inline">
                Length <input type="number" min="4" max="128" bind:value={genLength} />
              </label>
              <label class="inline"><input type="checkbox" bind:checked={genDigits} /> Digits</label>
              <label class="inline"><input type="checkbox" bind:checked={genSymbols} /> Symbols</label>
              <label class="inline">
                <input type="checkbox" bind:checked={genAvoidAmbiguous} /> No look-alikes
              </label>
            {:else}
              <label class="inline">
                Words <input type="number" min="3" max="12" bind:value={genWords} />
              </label>
            {/if}
            <button class="btn" onclick={runGenerator}>Generate</button>
          </div>
          {#if genValue}
            <div class="row">
              <code class="generated">{genValue}</code>
              <button class="btn" onclick={() => copy(genValue, 'Password', true)}>Copy</button>
              <button class="btn primary" onclick={useGenerated}>Use</button>
            </div>
          {/if}
        </div>
      {/if}

      {#if showComposer}
        <div class="panel">
          <h2>New item</h2>
          <div class="grid2">
            <label>
              Type
              <select bind:value={draftKind}>
                {#each Object.entries(KIND_LABELS) as [value, label]}
                  <option {value}>{label}</option>
                {/each}
              </select>
            </label>
            <label>Name<input bind:value={draftName} placeholder="GitHub" /></label>
            <label
              >Category<input
                bind:value={draftFolder}
                placeholder="Banking, Social, Infra…"
              /></label
            >
            <label>Username<input bind:value={draftUsername} placeholder="you@example.com" /></label>
            <label>Password<input type="password" bind:value={draftPassword} /></label>
            <label>URL<input bind:value={draftUri} placeholder="https://github.com" /></label>
            <label
              >Two-factor<input
                class="mono"
                bind:value={draftTotp}
                placeholder="otpauth:// or base32"
              /></label
            >
            <label class="span2">Notes<textarea rows="2" bind:value={draftNotes}></textarea></label>
          </div>
          <p class="muted small">
            Saving into <strong>{scopeLabel(itemScope)}</strong>.
          </p>
          <button class="btn primary" onclick={saveItem} disabled={busy}>Save item</button>
        </div>
      {/if}

      {#if filteredItems.length === 0}
        <p class="muted">
          {search
            ? 'Nothing matches that search.'
            : itemScope === GLOBAL_SCOPE
              ? 'No personal items yet. Add your first one above.'
              : 'This entity has no passwords yet. Add or import some above.'}
        </p>
      {:else}
        {#each groupedItems as group (group.folder)}
          {#if group.folder}
            <h3 class="category-head">{group.folder}</h3>
          {:else if groupedItems.length > 1}
            <h3 class="category-head muted">Uncategorised</h3>
          {/if}
          <ul class="items">
            {#each group.items as item (item.id)}
              <li class="item" class:open={openItemId === item.id}>
              <div class="item-head">
                <button
                  class="fav-star"
                  class:on={item.favorite}
                  title={item.favorite ? 'Unfavourite' : 'Favourite'}
                  onclick={() => toggleFavorite(item)}
                >
                  {item.favorite ? '★' : '☆'}
                </button>
                <button class="item-open" onclick={() => openItem(item.id)}>
                  <span class="item-name">{item.name}</span>
                  <span class="badge">{KIND_LABELS[item.kind]}</span>
                  {#if item.sharedFrom}
                    <span class="badge shared" title="Shared with you">from {item.sharedFrom}</span>
                  {/if}
                  {#if item.uris.length}<span class="uri">{item.uris[0]}</span>{/if}
                </button>
              </div>

              {#if openItemId === item.id && openSecret}
                <div class="detail">
                  <div class="field">
                    <span class="label">Username</span>
                    <input bind:value={openSecret.username} />
                    <button class="btn sm" onclick={() => copy(openSecret?.username, 'Username')}>
                      Copy
                    </button>
                  </div>
                  <div class="field">
                    <span class="label">Password</span>
                    {#if revealPassword}
                      <input bind:value={openSecret.password} />
                    {:else}
                      <input type="password" bind:value={openSecret.password} />
                    {/if}
                    <button class="btn sm" onclick={() => (revealPassword = !revealPassword)}>
                      {revealPassword ? 'Hide' : 'Reveal'}
                    </button>
                    <button
                      class="btn sm"
                      onclick={() => copy(openSecret?.password, 'Password', true)}
                    >
                      Copy
                    </button>
                  </div>

                  <!-- Password history: prior passwords the vault kept on each
                       change. Lets you undo a change you regret. -->
                  {#if openSecret.passwordHistory && openSecret.passwordHistory.length > 0}
                    <div class="field history-toggle">
                      <button class="link-btn" onclick={() => (showHistory = !showHistory)}>
                        {showHistory ? 'Hide' : 'Show'} previous passwords ({openSecret
                          .passwordHistory.length})
                      </button>
                    </div>
                    {#if showHistory}
                      <ul class="pw-history">
                        {#each openSecret.passwordHistory as entry (entry.changedAt + entry.password)}
                          <li>
                            <code class="pw-old">{entry.password}</code>
                            <span class="muted small">
                              until {new Date(entry.changedAt).toLocaleDateString()}
                            </span>
                            <button class="btn sm" onclick={() => copy(entry.password, 'Password', true)}>
                              Copy
                            </button>
                            <button class="btn sm" onclick={() => restorePassword(entry.password)}>
                              Restore
                            </button>
                          </li>
                        {/each}
                      </ul>
                    {/if}
                  {/if}

                  <!-- 2FA: the seed (`totpSecret`) is part of the secret half,
                       so it arrives with the item for editing — and is masked
                       like the password, because it mints every future code.
                       The live code below is computed on the gateway; reading
                       it does not count as activity, so an open item cannot
                       keep the vault unlocked. -->
                  <div class="field">
                    <span class="label">Two-factor</span>
                    {#if revealTotpSeed}
                      <input
                        class="mono grow"
                        placeholder="otpauth:// URI or base32 secret"
                        bind:value={openSecret.totpSecret}
                      />
                    {:else}
                      <input
                        type="password"
                        class="mono grow"
                        placeholder="otpauth:// URI or base32 secret"
                        bind:value={openSecret.totpSecret}
                      />
                    {/if}
                    <button class="btn sm" onclick={() => (revealTotpSeed = !revealTotpSeed)}>
                      {revealTotpSeed ? 'Hide' : 'Reveal'}
                    </button>
                  </div>
                  {#if totpProblem && !openTotp}
                    <p class="muted small">{totpProblem}</p>
                  {/if}
                  {#if openTotp}
                    <div class="field totp">
                      <span class="label">Code</span>
                      <code class="totp-code">
                        {openTotp.code.slice(0, 3)} {openTotp.code.slice(3)}
                      </code>
                      <span
                        class="totp-ring"
                        style="--frac:{openTotp.secondsRemaining / openTotp.period}"
                        title="{openTotp.secondsRemaining}s remaining"
                      >
                        {openTotp.secondsRemaining}s
                      </span>
                      <button class="btn sm" onclick={() => copy(openTotp?.code, 'Code', true)}>
                        Copy
                      </button>
                    </div>
                  {/if}

                  <!-- Custom fields: arbitrary name/value pairs (security
                       questions, account numbers, PINs). Hidden ones mask like a
                       password. Stored in the encrypted secret half. -->
                  {#if openSecret.customFields && openSecret.customFields.length > 0}
                    <div class="custom-fields">
                      {#each openSecret.customFields as cf, i (i)}
                        <div class="custom-field">
                          <input class="cf-name" placeholder="Field name" bind:value={cf.name} />
                          {#if cf.hidden}
                            <input class="cf-val" type="password" bind:value={cf.value} />
                          {:else}
                            <input class="cf-val" bind:value={cf.value} />
                          {/if}
                          <button
                            class="btn sm"
                            title={cf.hidden ? 'Show' : 'Hide'}
                            onclick={() => (cf.hidden = !cf.hidden)}
                          >
                            {cf.hidden ? '🙈' : '👁'}
                          </button>
                          <button class="btn sm" onclick={() => copy(cf.value, cf.name || 'Field', true)}>
                            Copy
                          </button>
                          <button class="btn sm danger" onclick={() => removeCustomField(i)}>×</button>
                        </div>
                      {/each}
                    </div>
                  {/if}

                  <div class="field">
                    <span class="label">Notes</span>
                    <textarea rows="2" bind:value={openSecret.notes}></textarea>
                  </div>
                  <div class="row wrap">
                    <button class="btn primary sm" onclick={saveOpenItem} disabled={busy}>Save</button>
                    <button class="btn sm" onclick={addCustomField}>+ Custom field</button>
                    <label class="move-to">
                      Move to
                      <select
                        value={item.scope}
                        onchange={(e) => moveItemToScope(item, e.currentTarget as HTMLSelectElement)}
                      >
                        <option value={GLOBAL_SCOPE}>Personal</option>
                        {#each itemScopeChips as s (s.scope)}
                          <option value={s.scope}>{s.label}</option>
                        {/each}
                      </select>
                    </label>
                    <button class="btn sm" onclick={() => openShare(item.id)}>Share…</button>
                    <button class="btn sm danger" onclick={() => removeItem(item.id, item.name)}>
                      Delete
                    </button>
                  </div>
                </div>
              {/if}
            </li>
            {/each}
          </ul>
        {/each}
      {/if}

      <details class="panel receive">
        <summary>Receive a shared item</summary>
        <p class="muted small">
          Give the sender your vault key, then paste the sealed envelope they send back. Only your
          vault can open it.
        </p>
        {#if myPublicKey}
          <div class="row">
            <code class="key">{myPublicKey}</code>
            <button class="btn sm" onclick={() => copy(myPublicKey, 'Vault key')}>Copy</button>
          </div>
        {:else}
          <button class="btn sm" onclick={async () => {
            const k = await guard(getPublicKey);
            if (k) myPublicKey = k;
          }}>Show my vault key</button>
        {/if}
        <textarea rows="4" placeholder="Paste envelope JSON…" bind:value={incomingEnvelope}></textarea>
        <button class="btn primary sm" onclick={receiveShare} disabled={busy}>Accept share</button>
      </details>
    {:else if tab === 'environment'}
      <div class="scope-bar">
        <button class:active={scope === GLOBAL_SCOPE} onclick={() => switchScope(GLOBAL_SCOPE)}>
          Global
        </button>
        {#each envScopeChips as s (s.scope)}
          <button class:active={scope === s.scope} onclick={() => switchScope(s.scope)}>
            {s.label} <span class="count">{s.count}</span>
          </button>
        {/each}
        {#if entityChips.length === 0}
          <span class="muted small">No entities yet — Global is the only environment.</span>
        {/if}
      </div>

      <div class="toolbar">
        <p class="muted small">
          {#if scope === GLOBAL_SCOPE}
            Account-wide defaults. Every entity inherits these unless it sets its own —
            <code>OPENAI_API_KEY</code> here and every agent picks it up.
          {:else}
            Only this entity sees these, and they shadow the global value of the same name.
            Two entities can hold different <code>STRIPE_SECRET_KEY</code> values without either
            being able to read the other's.
          {/if}
          Values are never returned by the list endpoint; reveal fetches one at a time.
        </p>
      </div>

      <div class="panel">
        <div class="row">
          <input class="mono" placeholder="KEY_NAME" bind:value={newSecretKey} />
          <input class="mono grow" placeholder="value" bind:value={newSecretValue} />
          <button class="btn primary" onclick={saveSecret} disabled={busy}>Set</button>
        </div>
      </div>

      {#if secretKeys.length === 0}
        <p class="muted">
          {scope === GLOBAL_SCOPE
            ? 'No environment variables stored yet.'
            : 'Nothing set for this entity yet — it inherits everything from Global.'}
        </p>
      {:else}
        <ul class="secrets">
          {#each secretKeys as key (key)}
            <li>
              <code class="skey">{key}</code>
              {#if revealedSecrets[key] !== undefined}
                <code class="sval">{revealedSecrets[key]}</code>
              {:else}
                <span class="sval masked">••••••••••••</span>
              {/if}
              <button class="btn sm" onclick={() => revealSecret(key)}>
                {revealedSecrets[key] !== undefined ? 'Hide' : 'Reveal'}
              </button>
              <button
                class="btn sm"
                onclick={async () => {
                  const v = revealedSecrets[key] ?? (await guard(() => getSecret(key, scope)));
                  if (v) copy(v, key, true);
                }}
              >
                Copy
              </button>
              <button class="btn sm danger" onclick={() => removeSecret(key)}>Delete</button>
            </li>
          {/each}
        </ul>
      {/if}
    {:else}
      <!-- Security: password health, the ways in, and the backup that outlives them. -->
      {#if health}
        <div class="panel health">
          <div class="health-head">
            <div
              class="score-ring"
              style="--pct:{health.score}"
              data-band={health.score >= 80 ? 'good' : health.score >= 50 ? 'fair' : 'poor'}
            >
              <span>{health.score}</span>
            </div>
            <div>
              <h2>Password health</h2>
              <p class="muted small">
                {#if health.totalWithPasswords === 0}
                  No passwords to check yet.
                {:else if health.weak.length + health.reused.length + health.stale.length === 0}
                  All {health.totalWithPasswords} passwords look healthy. Nice.
                {:else}
                  Across {health.totalWithPasswords} passwords: {health.weak.length} weak, {health
                    .reused.length} reused, {health.stale.length} old.
                {/if}
              </p>
            </div>
          </div>

          {#each [{ key: 'weak', label: 'Weak', list: health.weak }, { key: 'reused', label: 'Reused', list: health.reused }, { key: 'stale', label: 'Old', list: health.stale }] as group (group.key)}
            {#if group.list.length}
              <div class="health-group">
                <h3>{group.label} <span class="count">{group.list.length}</span></h3>
                <ul class="flags">
                  {#each group.list as flag (flag.id + group.key)}
                    <li>
                      <button class="flag" onclick={() => goToItem(flag)}>
                        <span class="flag-name">{flag.name}</span>
                        {#if flag.scope !== GLOBAL_SCOPE}
                          <span class="pill">{scopeLabel(flag.scope)}</span>
                        {/if}
                        <span class="muted small">{flag.detail}</span>
                      </button>
                    </li>
                  {/each}
                </ul>
              </div>
            {/if}
          {/each}

          <!-- Breach check against Have I Been Pwned. Separate from the offline
               health checks above because it reaches the network. -->
          <div class="health-group breach">
            <div class="breach-head">
              <div>
                <h3>Data breaches</h3>
                <p class="muted small">
                  Checks each password against Have I Been Pwned. Only a 5-character hash prefix
                  per password ever leaves your machine.
                </p>
              </div>
              <button class="btn sm" onclick={runBreachCheck} disabled={breachRunning}>
                {breachRunning ? 'Checking…' : 'Check now'}
              </button>
            </div>

            {#if breach}
              {#if breach.breached.length > 0}
                <p class="muted small breach-warn">
                  {breach.breached.length} of {breach.checked} passwords appear in known breaches — change
                  these first.
                </p>
                <ul class="flags">
                  {#each breach.breached as b (b.id)}
                    <li>
                      <button class="flag" onclick={() => goToItem(b)}>
                        <span class="flag-name">{b.name}</span>
                        {#if b.scope !== GLOBAL_SCOPE}
                          <span class="pill">{scopeLabel(b.scope)}</span>
                        {/if}
                        <span class="muted small">Seen {b.count.toLocaleString()} times in breaches</span>
                      </button>
                    </li>
                  {/each}
                </ul>
              {:else}
                <p class="muted small">
                  Checked {breach.checked} passwords — none found in known breaches.
                </p>
              {/if}
              {#if breach.unreachable}
                <p class="muted small">
                  Some checks couldn't reach the breach service; try again when you're online.
                </p>
              {/if}
            {/if}
          </div>
        </div>
      {/if}

      <div class="panel">
        <h2>Ways in</h2>
        <p class="muted small">
          Each one wraps the same key independently. Adding or removing one never re-encrypts
          your data, and the last remaining one cannot be removed.
        </p>

        <ul class="methods">
          {#each status.unlockMethods as method (method.id)}
            <li>
              <div class="method-main">
                <strong>{method.label}</strong>
                <span class="pill">{UNLOCK_KIND_LABELS[method.kind]}</span>
              </div>
              <span class="muted small">
                Added {new Date(method.createdAt).toLocaleDateString()}
                {#if method.lastUsedAt}
                  · last used {new Date(method.lastUsedAt).toLocaleDateString()}
                {:else}
                  · never used
                {/if}
              </span>
              <button
                class="btn sm danger"
                disabled={status.unlockMethods.length <= 1}
                onclick={() => revokeMethod(method)}
              >
                Revoke
              </button>
            </li>
          {/each}
        </ul>

        <div class="row wrap">
          {#if desktopSupported}
            <!-- Embedded webviews do not support the PRF extension dependably,
                 so the desktop shell enrols an OS-keychain key instead. -->
            {#if computerReady}
              <button class="btn danger" onclick={removeThisComputer} disabled={busy}>
                Remove this computer
              </button>
            {:else}
              <button class="btn primary" onclick={addThisComputer} disabled={busy}>
                Add this computer
              </button>
            {/if}
          {:else if passkeySupported}
            <input placeholder="Passkey name, e.g. iPhone" bind:value={newPasskeyLabel} />
            <button class="btn primary" onclick={addPasskey} disabled={busy}>
              Add a passkey
            </button>
          {:else}
            <p class="muted small">This browser does not support passkeys.</p>
          {/if}
          <button class="btn" onclick={mintRecoveryCode} disabled={busy}>
            New recovery code
          </button>
          <button class="btn" onclick={() => (showRotate = !showRotate)}>
            Change master password
          </button>
        </div>
      </div>

      <div class="panel">
        <h2>Backup</h2>
        <p class="muted small">
          A backup opens with its passphrase alone — no device, no passkey, no copy of this
          vault. Use a different passphrase from your master password, so one compromise is not
          two. Restoring only adds: nothing already here is overwritten.
        </p>
        <div class="row wrap">
          <input
            type="password"
            placeholder="Backup passphrase"
            bind:value={backupPassphrase}
          />
          <button class="btn primary" onclick={downloadBackup} disabled={busy}>
            Download backup
          </button>
        </div>

        <details>
          <summary>Restore from a backup</summary>
          <div class="row wrap">
            <input type="file" accept="application/json" onchange={onBackupFile} />
            <input
              type="password"
              placeholder="Backup passphrase"
              bind:value={restorePassphrase}
            />
            <button class="btn" onclick={restoreBackup} disabled={busy || !restoreJson}>
              Restore
            </button>
          </div>
        </details>
      </div>

      <div class="panel">
        <h2>Activity</h2>
        <p class="muted small">
          Kept inside the encrypted vault. Records what changed and when, never any value.
        </p>
        {#if auditEntries.length === 0}
          <p class="muted">Nothing recorded yet.</p>
        {:else}
          <ul class="audit">
            {#each auditEntries.slice(0, 50) as entry, i (entry.at + i)}
              <li>
                <code class="action">{entry.action}</code>
                {#if entry.detail}<span class="detail">{entry.detail}</span>{/if}
                <span class="muted small">{new Date(entry.at).toLocaleString()}</span>
              </li>
            {/each}
          </ul>
        {/if}
      </div>
    {/if}

    {#if showShare}
      <div class="modal-backdrop" role="presentation" onclick={() => (showShare = false)}></div>
      <div class="modal" role="dialog" aria-label="Share item">
        <h2>Share securely</h2>
        <p class="muted small">
          The item is sealed to the recipient's vault key. The gateway relays it but cannot read
          it, and your master password is never involved.
        </p>
        <label>
          Recipient's vault key
          <input class="mono" bind:value={shareRecipientKey} placeholder="base64 public key" />
        </label>
        <label>
          Scenario (optional)
          <input bind:value={shareScenario} placeholder="on-call handover" />
        </label>
        <label>
          Expires after
          <select bind:value={shareExpiryHours}>
            <option value={1}>1 hour</option>
            <option value={24}>24 hours</option>
            <option value={168}>7 days</option>
            <option value={0}>No expiry</option>
          </select>
        </label>
        <div class="row">
          <button class="btn primary" onclick={buildShare} disabled={busy}>Create envelope</button>
          <button class="btn" onclick={() => (showShare = false)}>Close</button>
        </div>
        {#if shareEnvelopeJson}
          <p class="muted small">Send this to the recipient:</p>
          <textarea rows="6" readonly class="mono">{shareEnvelopeJson}</textarea>
          <button class="btn sm" onclick={() => copy(shareEnvelopeJson, 'Envelope')}>
            Copy envelope
          </button>
        {/if}
      </div>
    {/if}
  {/if}
</div>

<style>
  .vault {
    max-width: 1080px;
    margin: 0 auto;
    padding: var(--page-padding);
    display: grid;
    gap: var(--space-3);
  }

  .head {
    display: flex;
    justify-content: space-between;
    align-items: flex-start;
    gap: var(--space-3);
    flex-wrap: wrap;
  }
  h1 {
    font-size: var(--text-xl);
    font-weight: var(--font-semibold);
    color: var(--heading-contrast);
    margin: 0;
  }
  h2 {
    font-size: var(--text-md);
    font-weight: var(--font-semibold);
    color: var(--heading-contrast);
    margin: 0 0 var(--space-2);
  }
  .sub {
    margin: 4px 0 0;
    color: var(--text-secondary);
    font-size: var(--text-base);
    max-width: 62ch;
  }
  .head-actions {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .countdown {
    font-size: var(--text-sm);
    color: var(--text-muted);
  }

  .muted {
    color: var(--text-secondary);
    font-size: var(--text-base);
  }
  .small {
    font-size: var(--text-sm);
  }

  .banner {
    padding: 8px 12px;
    border-radius: var(--radius-md);
    font-size: var(--text-base);
    border: 1px solid var(--border);
  }
  .banner.error {
    background: var(--color-red-dim, rgba(180, 70, 60, 0.14));
    color: var(--text-primary);
  }
  .banner.ok {
    background: var(--surface-accent-soft);
    color: var(--text-primary);
  }

  .panel {
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    background: var(--panel-gradient);
    padding: var(--space-3);
    display: grid;
    gap: var(--space-2);
  }
  .gate {
    max-width: 420px;
    justify-self: center;
    width: 100%;
    text-align: left;
  }

  .tabs {
    display: flex;
    gap: var(--space-1);
    border-bottom: 1px solid var(--border);
  }
  .tabs button {
    background: none;
    border: none;
    border-bottom: 2px solid transparent;
    color: var(--text-secondary);
    padding: 8px 12px;
    cursor: pointer;
    font-size: var(--text-base);
  }
  .tabs button.active {
    color: var(--text-primary);
    border-bottom-color: var(--accent-orange, #c98a4b);
  }
  .count {
    color: var(--text-muted);
    font-size: var(--text-sm);
  }

  .toolbar {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    flex-wrap: wrap;
  }
  .search {
    flex: 1;
    min-width: 200px;
  }

  /* ---- scope switcher (Environment) ---- */
  .scope-bar {
    display: flex;
    gap: var(--space-2);
    align-items: center;
    flex-wrap: wrap;
    padding-bottom: var(--space-2);
  }
  .scope-bar button:not(.btn) {
    background: transparent;
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 4px 12px;
    color: var(--text-muted);
    font: inherit;
    font-size: var(--text-sm);
    cursor: pointer;
  }
  .scope-bar button:not(.btn).active {
    color: var(--text-primary);
    border-color: var(--accent-orange, #c98a4b);
  }

  /* ---- unlock gate extras ---- */
  .btn.big {
    padding: 10px 18px;
    font-size: var(--text-base);
  }
  .divider {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    color: var(--text-muted);
    font-size: var(--text-sm);
    width: 100%;
  }
  .divider::before,
  .divider::after {
    content: '';
    flex: 1;
    height: 1px;
    background: var(--border);
  }
  .link-btn {
    background: none;
    border: none;
    color: var(--text-muted);
    font: inherit;
    font-size: var(--text-sm);
    text-decoration: underline;
    cursor: pointer;
    padding: 0;
  }
  .recovery-unlock {
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
    width: 100%;
  }

  /* ---- the once-only recovery code ---- */
  .panel.recovery {
    border-color: var(--accent-orange, #c98a4b);
  }
  .recovery-code {
    display: block;
    padding: var(--space-3);
    margin: var(--space-2) 0;
    background: var(--bg-page, rgba(0, 0, 0, 0.2));
    border-radius: var(--radius-md);
    font-size: 1.05rem;
    letter-spacing: 0.06em;
    word-break: break-all;
    user-select: all;
  }
  .ack {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    font-size: var(--text-sm);
    color: var(--text-muted);
    margin: var(--space-2) 0;
  }
  .ack input {
    width: auto;
  }

  /* ---- password health ---- */
  .health-head {
    display: flex;
    align-items: center;
    gap: var(--space-4);
  }
  .health-head h2 {
    margin: 0;
  }
  .score-ring {
    --pct: 100;
    --ring: #3fae5a;
    flex: 0 0 auto;
    width: 64px;
    height: 64px;
    border-radius: 50%;
    display: grid;
    place-items: center;
    background: conic-gradient(
      var(--ring) calc(var(--pct) * 1%),
      var(--border) calc(var(--pct) * 1%)
    );
    font-weight: 700;
  }
  .score-ring span {
    width: 50px;
    height: 50px;
    border-radius: 50%;
    background: var(--bg-card);
    display: grid;
    place-items: center;
  }
  .score-ring[data-band='fair'] {
    --ring: #e0a106;
  }
  .score-ring[data-band='poor'] {
    --ring: #d9534f;
  }
  .health-group {
    margin-top: var(--space-3);
  }
  .health-group.breach {
    border-top: 1px solid var(--border);
    padding-top: var(--space-3);
  }
  .breach-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: var(--space-3);
  }
  .breach-head h3 {
    margin: 0 0 2px;
    text-transform: none;
    letter-spacing: 0;
    font-size: 1rem;
    color: var(--text-primary);
  }
  .breach-warn {
    color: #d9534f;
  }

  /* ---- password history ---- */
  .history-toggle {
    margin-top: 4px;
  }
  .pw-history {
    list-style: none;
    padding: 0;
    margin: 4px 0 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .pw-history li {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex-wrap: wrap;
  }
  .pw-old {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    background: var(--bg-page, rgba(0, 0, 0, 0.15));
    padding: 2px 8px;
    border-radius: var(--radius-sm, 6px);
    word-break: break-all;
  }
  .health-group h3 {
    font-size: var(--text-sm);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
    margin: 0 0 var(--space-1);
  }
  .flags {
    list-style: none;
    padding: 0;
    margin: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .flag {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    width: 100%;
    text-align: left;
    background: transparent;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    padding: 8px 12px;
    color: var(--text-primary);
    font: inherit;
    cursor: pointer;
  }
  .flag:hover {
    border-color: var(--accent-orange, #c98a4b);
  }
  .flag-name {
    font-weight: 500;
  }

  /* ---- security tab ---- */
  .methods,
  .audit {
    list-style: none;
    padding: 0;
    margin: var(--space-3) 0;
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .methods li {
    display: flex;
    align-items: center;
    gap: var(--space-3);
    padding: var(--space-2) 0;
    border-bottom: 1px solid var(--border);
    flex-wrap: wrap;
  }
  .method-main {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    flex: 1;
    min-width: 160px;
  }
  .pill {
    border: 1px solid var(--border);
    border-radius: 999px;
    padding: 1px 8px;
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .audit li {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
    flex-wrap: wrap;
    font-size: var(--text-sm);
  }
  .audit .action {
    color: var(--accent-orange, #c98a4b);
  }
  .audit .detail {
    flex: 1;
    color: var(--text-primary);
    overflow-wrap: anywhere;
  }

  /* ---- master password strength meter ---- */
  .strength {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .strength-bars {
    display: flex;
    gap: 4px;
  }
  .strength-bars span {
    flex: 1;
    height: 4px;
    border-radius: 2px;
    background: var(--border);
    transition: background 0.2s;
  }
  /* Colour the filled bars by score: red → amber → green. */
  .strength[data-score='0'] .strength-bars span.filled,
  .strength[data-score='1'] .strength-bars span.filled {
    background: #d9534f;
  }
  .strength[data-score='2'] .strength-bars span.filled {
    background: #e0a106;
  }
  .strength[data-score='3'] .strength-bars span.filled,
  .strength[data-score='4'] .strength-bars span.filled {
    background: #3fae5a;
  }
  .strength-text {
    display: flex;
    align-items: baseline;
    gap: var(--space-2);
  }

  /* ---- category (folder) headings within a scope ---- */
  .category-head {
    font-size: var(--text-sm);
    text-transform: uppercase;
    letter-spacing: 0.05em;
    color: var(--text-muted);
    margin: var(--space-3) 0 var(--space-1);
  }
  .move-to {
    display: inline-flex;
    align-items: center;
    gap: var(--space-1);
    font-size: var(--text-sm);
    color: var(--text-muted);
  }
  .move-to select {
    width: auto;
  }

  /* ---- live TOTP code ---- */
  .field.totp {
    align-items: center;
  }
  .totp-code {
    font-size: 1.25rem;
    letter-spacing: 0.12em;
    font-variant-numeric: tabular-nums;
    color: var(--accent-orange, #c98a4b);
  }
  .totp-ring {
    font-size: var(--text-sm);
    color: var(--text-muted);
    font-variant-numeric: tabular-nums;
    min-width: 3ch;
    text-align: right;
  }

  /* ---- import preview ---- */
  .import-preview {
    margin-top: var(--space-3);
    padding-top: var(--space-3);
    border-top: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    gap: var(--space-2);
  }
  .import-preview ul {
    max-height: 160px;
    overflow-y: auto;
    padding-left: var(--space-4);
  }

  input,
  select,
  textarea {
    padding: 7px 10px;
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    font-size: var(--text-base);
    font-family: inherit;
    width: 100%;
  }
  .mono,
  code {
    font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
    font-size: var(--text-sm);
  }
  .grow {
    flex: 1;
  }

  .row {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .row.wrap {
    flex-wrap: wrap;
  }
  .inline {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    white-space: nowrap;
    color: var(--text-secondary);
    font-size: var(--text-sm);
    width: auto;
  }
  .inline input[type='number'] {
    width: 72px;
  }
  .inline input[type='checkbox'] {
    width: auto;
  }

  .grid2 {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: var(--space-2);
  }
  .grid2 label,
  .modal label {
    display: grid;
    gap: 4px;
    font-size: var(--text-sm);
    color: var(--text-secondary);
  }
  .span2 {
    grid-column: 1 / -1;
  }

  .btn {
    padding: 0 12px;
    height: var(--btn-height-md);
    border-radius: var(--radius-md);
    border: 1px solid var(--border);
    background: var(--bg-card);
    color: var(--text-primary);
    cursor: pointer;
    font-size: var(--text-base);
    white-space: nowrap;
  }
  .btn:hover {
    background: var(--bg-card-hover);
  }
  .btn.primary {
    background: var(--chat-user-bg);
    border-color: transparent;
    color: var(--chat-user-text);
  }
  .btn.sm {
    height: var(--btn-height-sm);
    font-size: var(--text-sm);
  }
  .btn.danger {
    color: var(--accent-red, #b4463c);
  }
  .btn:disabled {
    opacity: 0.55;
    cursor: not-allowed;
  }

  .items,
  .secrets {
    list-style: none;
    margin: 0;
    padding: 0;
    display: grid;
    gap: 6px;
  }
  .item {
    border: 1px solid var(--border);
    border-radius: var(--radius-lg);
    background: var(--stack-card-bg);
    overflow: hidden;
  }
  .item.open {
    border-color: var(--color-purple-border, rgba(201, 138, 75, 0.48));
  }
  .item-head {
    width: 100%;
    display: flex;
    align-items: center;
    gap: 2px;
  }
  .item-head:hover {
    background: var(--surface-hover);
  }
  .item-open {
    flex: 1;
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 10px 12px 10px 4px;
    background: none;
    border: none;
    color: var(--text-primary);
    cursor: pointer;
    text-align: left;
    font-size: var(--text-base);
  }
  .fav-star {
    background: none;
    border: none;
    cursor: pointer;
    font-size: 1.1rem;
    line-height: 1;
    padding: 10px 4px 10px 10px;
    color: var(--text-muted);
  }
  .fav-star.on {
    color: #e0a106;
  }

  /* ---- custom fields ---- */
  .custom-fields {
    display: flex;
    flex-direction: column;
    gap: 4px;
  }
  .custom-field {
    display: flex;
    align-items: center;
    gap: var(--space-2);
  }
  .cf-name {
    max-width: 160px;
  }
  .cf-val {
    flex: 1;
  }
  .item-name {
    font-weight: var(--font-medium);
  }
  .badge {
    font-size: var(--text-sm);
    color: var(--text-muted);
    border: 1px solid var(--border);
    border-radius: var(--radius-full);
    padding: 1px 8px;
  }
  .badge.shared {
    color: var(--text-secondary);
    background: var(--surface-accent-soft);
  }
  .uri {
    color: var(--text-muted);
    font-size: var(--text-sm);
    margin-left: auto;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    max-width: 40%;
  }

  .detail {
    padding: 0 12px 12px;
    display: grid;
    gap: var(--space-2);
    border-top: 1px solid var(--border);
  }
  .field {
    display: flex;
    gap: var(--space-2);
    align-items: center;
  }
  .field .label {
    width: 84px;
    flex: none;
    color: var(--text-secondary);
    font-size: var(--text-sm);
  }

  .generated {
    flex: 1;
    padding: 8px 10px;
    background: var(--surface-sunken);
    border-radius: var(--radius-md);
    word-break: break-all;
  }
  .key {
    flex: 1;
    word-break: break-all;
    color: var(--text-secondary);
  }

  .secrets li {
    display: flex;
    align-items: center;
    gap: var(--space-2);
    padding: 8px 12px;
    border: 1px solid var(--border);
    border-radius: var(--radius-md);
    background: var(--stack-card-bg);
  }
  .skey {
    min-width: 200px;
    color: var(--text-primary);
  }
  .sval {
    flex: 1;
    color: var(--text-secondary);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .masked {
    letter-spacing: 2px;
  }

  .receive summary {
    cursor: pointer;
    color: var(--text-secondary);
    font-size: var(--text-base);
  }

  .modal-backdrop {
    position: fixed;
    inset: 0;
    background: var(--color-bg-overlay, rgba(10, 9, 8, 0.78));
    z-index: var(--z-overlay);
  }
  .modal {
    position: fixed;
    top: 50%;
    left: 50%;
    transform: translate(-50%, -50%);
    width: min(560px, calc(100vw - 32px));
    max-height: calc(100vh - 64px);
    overflow-y: auto;
    z-index: var(--z-modal);
    background: var(--bg-card);
    border: 1px solid var(--border);
    border-radius: var(--radius-xl);
    box-shadow: var(--shadow-xl);
    padding: var(--space-4);
    display: grid;
    gap: var(--space-2);
  }

  @media (max-width: 720px) {
    .grid2 {
      grid-template-columns: 1fr;
    }
    .field {
      flex-wrap: wrap;
    }
    .skey {
      min-width: 0;
    }
    .secrets li {
      flex-wrap: wrap;
    }
  }
</style>
