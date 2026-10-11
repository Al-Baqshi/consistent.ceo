import { CameraView, useCameraPermissions } from 'expo-camera';
import { useRouter } from 'expo-router';
import React, { useEffect, useRef, useState } from 'react';
import { Linking, Platform, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { pingGateway, setGatewayUrl } from '@/api';
import { auth } from '@/api/auth';
import { APIError } from '@/api/client';
import { IconButton, NativeButton, Txt } from '@/components';
import { haptics } from '@/lib/haptics';
import { isLocalNetworkUrl } from '@/lib/links';
import { spacedCode } from '@/lib/pairingKey';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

/**
 * Scan-to-connect. The desktop/CLI renders a QR code with
 * `{ "url": <base url>, "workspace": <id>, "v": 1, "nonce"?: <one-time code> }`.
 * First-time pairing has no token yet, which is why the payload travels in
 * the QR itself rather than being fetched.
 *
 * Flow: scan → parse+validate → probe `GET /health` on that URL → store it
 * via `setGatewayUrl` (only after the health check passes, so a bad scan
 * can't clobber a working configuration). Then:
 *
 * - **With a `nonce`** (pairing by presence, `crates/gateway/src/pairing.rs`):
 *   make this phone's key, show six digits, and wait for the person to press
 *   Allow on the computer after checking the same digits there. The phone is
 *   then signed in, and signs itself back in later with its key.
 * - **Without** (an older desktop): route to sign-in as before.
 */

type Phase =
  | { kind: 'scanning' }
  | { kind: 'checking'; url: string }
  | { kind: 'confirm'; code: string }
  | { kind: 'success'; url: string }
  | { kind: 'error'; message: string };

function parsePayload(text: string): { url: string; nonce?: string } | { error: string } {
  let raw: unknown;
  try {
    raw = JSON.parse(text);
  } catch {
    return { error: "That QR code isn't a Consistent CEO pairing code." };
  }
  const rec = (raw ?? {}) as Record<string, unknown>;
  if (rec.v !== 1) {
    return { error: 'Unsupported pairing code version — update the app and the desktop, then try again.' };
  }
  const url = typeof rec.url === 'string' ? rec.url.trim().replace(/\/+$/, '') : '';
  if (!/^https?:\/\/.+/i.test(url)) {
    return { error: "The pairing code doesn't carry a valid address for your computer." };
  }
  const nonce = typeof rec.nonce === 'string' && rec.nonce.length > 0 ? rec.nonce : undefined;
  return { url, nonce };
}

export default function PairScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();
  const [permission, requestPermission] = useCameraPermissions();
  const [phase, setPhase] = useState<Phase>({ kind: 'scanning' });
  // `onBarcodeScanned` fires repeatedly while the code stays in frame —
  // handle exactly one scan until the operator asks to scan again.
  const handled = useRef(false);
  // Stops the approval wait when the screen closes or a new scan starts.
  const waitToken = useRef(0);
  useEffect(() => () => void (waitToken.current += 1), []);

  // Ask as soon as the screen opens; the operator came here to use the camera.
  useEffect(() => {
    if (permission && !permission.granted && permission.canAskAgain) {
      void requestPermission();
    }
  }, [permission, requestPermission]);

  const onScanned = async ({ data }: { data: string }) => {
    if (handled.current) return;
    handled.current = true;
    const parsed = parsePayload(data);
    if ('error' in parsed) {
      haptics.warning();
      setPhase({ kind: 'error', message: parsed.error });
      return;
    }
    setPhase({ kind: 'checking', url: parsed.url });
    const lan = isLocalNetworkUrl(parsed.url);
    let healthy = await pingGateway(parsed.url);
    // On the home network the very first request is what makes iOS ask for
    // Local Network access, and it fails while that prompt is on screen. Give
    // the operator a moment to answer, then try once more.
    if (!healthy && lan) {
      await new Promise((r) => setTimeout(r, 1500));
      healthy = await pingGateway(parsed.url);
    }
    if (!healthy) {
      haptics.warning();
      setPhase({
        kind: 'error',
        message: lan
          ? `Scanned ${parsed.url}, but your computer didn't answer. Make sure this phone is on the same Wi-Fi as your computer. If iOS asked to find devices on your local network, allow it (Settings → Privacy & Security → Local Network → Consistent CEO), then tap Scan again.`
          : `Scanned ${parsed.url}, but your computer didn't answer. Make sure Consistent CEO is open on it and this phone can reach that address, then try again.`,
      });
      return;
    }
    await setGatewayUrl(parsed.url);
    if (parsed.nonce) {
      await pairByPresence(parsed.url, parsed.nonce);
      return;
    }
    haptics.success();
    if (!(await auth.isSignedIn())) {
      // Connected, but this phone has no session yet — straight to sign-in.
      router.replace('/login');
      return;
    }
    setPhase({ kind: 'success', url: parsed.url });
    setTimeout(() => router.back(), 1200);
  };

  const pairByPresence = async (url: string, nonce: string) => {
    const label = Platform.OS === 'ios' ? 'iPhone' : 'Android phone';
    let pairing: Awaited<ReturnType<typeof auth.startPairing>>;
    try {
      pairing = await auth.startPairing(nonce, label, Platform.OS);
    } catch (e) {
      haptics.warning();
      const gone = e instanceof APIError && e.status === 410;
      setPhase({
        kind: 'error',
        message: gone
          ? 'That code was already used or has expired. Make a new one on your computer (Settings → Your phone → Pair a phone), then scan again.'
          : e instanceof APIError
            ? `Your computer refused the code (${e.status}). Make a new one and scan again.`
            : 'This version of the app can’t pair by code. Sign in with your username and password instead, or update the app.',
      });
      if (!(e instanceof APIError)) setTimeout(() => router.replace('/login'), 2500);
      return;
    }
    haptics.success();
    setPhase({ kind: 'confirm', code: pairing.code });
    const mine = ++waitToken.current;
    const deadline = Date.now() + 3 * 60_000;
    while (Date.now() < deadline) {
      await new Promise((r) => setTimeout(r, 2000));
      if (waitToken.current !== mine) return;
      try {
        const status = await auth.collectPairing(pairing.requestId, pairing.key);
        if (status === 'approved') {
          haptics.success();
          setPhase({ kind: 'success', url });
          setTimeout(() => router.back(), 1200);
          return;
        }
        if (status === 'denied') {
          haptics.warning();
          setPhase({ kind: 'error', message: 'Your computer said no. If that was a mistake, make a new code and scan again.' });
          return;
        }
      } catch (e) {
        if (e instanceof APIError && (e.status === 410 || e.status === 401)) {
          haptics.warning();
          setPhase({ kind: 'error', message: 'The request expired before it was allowed. Make a new code and scan again.' });
          return;
        }
        // Network blip: keep waiting.
      }
    }
    if (waitToken.current === mine) {
      setPhase({ kind: 'error', message: 'Nobody pressed Allow in time. Make a new code on your computer and scan again.' });
    }
  };

  const scanAgain = () => {
    haptics.tap();
    handled.current = false;
    waitToken.current += 1;
    setPhase({ kind: 'scanning' });
  };

  const cameraBlocked =
    Platform.OS === 'web' || (permission != null && !permission.granted && !permission.canAskAgain);

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top }}>
      <View
        style={{
          flexDirection: 'row',
          alignItems: 'center',
          justifyContent: 'space-between',
          paddingHorizontal: spacing[5],
          paddingVertical: spacing[3],
        }}
      >
        <Txt variant="title">Scan to connect</Txt>
        <IconButton icon="✕" filled onPress={() => router.back()} label="Close" />
      </View>

      <View style={{ flex: 1, paddingHorizontal: spacing[5], paddingBottom: insets.bottom + spacing[6], gap: spacing[4] }}>
        {cameraBlocked ? (
          // Camera unavailable (web) or permanently denied — explain and fall
          // back to manual entry rather than showing a black viewfinder.
          <View style={{ gap: spacing[4], paddingTop: spacing[6] }}>
            <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
              {Platform.OS === 'web'
                ? 'QR scanning isn’t available on web. Type your computer’s address into Settings instead.'
                : 'Camera access is denied, so the pairing code can’t be scanned. Allow the camera for Consistent CEO in Settings, or type your computer’s address by hand.'}
            </Txt>
            {Platform.OS !== 'web' ? (
              <NativeButton
                label="Open device settings"
                icon="⛭"
                variant="outline"
                fullWidth
                onPress={() => void Linking.openSettings()}
              />
            ) : null}
            <NativeButton label="Enter the URL manually" variant="ghost" fullWidth onPress={() => router.back()} />
          </View>
        ) : !permission?.granted ? (
          <View style={{ gap: spacing[4], paddingTop: spacing[6] }}>
            <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
              Consistent CEO needs the camera to scan the pairing QR code shown on your computer.
              Nothing is recorded — the camera only reads the code.
            </Txt>
            <NativeButton label="Allow camera" icon="◉" fullWidth onPress={() => void requestPermission()} />
            <NativeButton label="Enter the URL manually" variant="ghost" fullWidth onPress={() => router.back()} />
          </View>
        ) : (
          <>
            <View
              style={{
                flex: 1,
                borderRadius: radius['2xl'],
                overflow: 'hidden',
                borderWidth: 1,
                borderColor: theme.colors.border,
                backgroundColor: theme.colors.bgSurface,
              }}
            >
              <CameraView
                style={{ flex: 1 }}
                facing="back"
                barcodeScannerSettings={{ barcodeTypes: ['qr'] }}
                onBarcodeScanned={phase.kind === 'scanning' ? (e) => void onScanned(e) : undefined}
              />
            </View>

            {phase.kind === 'scanning' ? (
              <Txt variant="caption" tone="muted" center>
                Point the camera at the QR code in Consistent CEO on your computer.
              </Txt>
            ) : null}

            {phase.kind === 'checking' ? (
              <StatusLine tone="muted" text={`Checking ${phase.url}…`} />
            ) : null}

            {phase.kind === 'confirm' ? (
              <View style={{ gap: spacing[2], alignItems: 'center' }}>
                <Txt variant="caption" tone="muted" center>
                  On your computer, check it shows this number, then click Allow.
                </Txt>
                <Txt
                  variant="title"
                  center
                  style={{ fontSize: 40, letterSpacing: 2, fontVariant: ['tabular-nums'] }}
                  accessibilityLabel={`Confirmation number ${phase.code.split('').join(' ')}`}
                >
                  {spacedCode(phase.code)}
                </Txt>
                <Txt variant="caption" tone="muted" center>
                  Waiting for your computer…
                </Txt>
              </View>
            ) : null}

            {phase.kind === 'success' ? (
              <StatusLine tone="success" text="Connected ✓ — this phone now talks to your computer." />
            ) : null}

            {phase.kind === 'error' ? (
              <View style={{ gap: spacing[3] }}>
                <Txt variant="body" style={{ color: theme.colors.danger, lineHeight: 21 }}>
                  {phase.message}
                </Txt>
                <View style={{ flexDirection: 'row', gap: spacing[2] }}>
                  <NativeButton label="Enter manually" variant="ghost" size="sm" style={{ flex: 1 }} onPress={() => router.back()} />
                  <NativeButton label="Scan again" size="sm" icon="↻" style={{ flex: 1.4 }} onPress={scanAgain} />
                </View>
              </View>
            ) : null}
          </>
        )}
      </View>
    </View>
  );
}

function StatusLine({ tone, text }: { tone: 'muted' | 'success'; text: string }) {
  const theme = useTheme();
  return (
    <View
      style={{
        borderRadius: radius.lg,
        backgroundColor: tone === 'success' ? theme.colors.success + '22' : theme.colors.bgCard,
        borderWidth: 1,
        borderColor: tone === 'success' ? theme.colors.success + '44' : theme.colors.border,
        paddingVertical: spacing[3],
        paddingHorizontal: spacing[4],
        alignItems: 'center',
      }}
    >
      <Txt variant="label" style={tone === 'success' ? { color: theme.colors.success } : undefined} tone={tone === 'success' ? undefined : 'muted'}>
        {text}
      </Txt>
    </View>
  );
}
