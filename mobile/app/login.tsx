import { useRouter } from 'expo-router';
import React, { useEffect, useState } from 'react';
import { Platform, TextInput, View } from 'react-native';
import { useSafeAreaInsets } from 'react-native-safe-area-context';

import { APIError, getGatewayUrl, isDemoGateway } from '@/api/client';
import { auth } from '@/api/auth';
import { NativeButton, Txt } from '@/components';
import { plainHttpNote } from '@/lib/links';
import { useTheme } from '@/theme/ThemeProvider';
import { radius, spacing } from '@/theme/tokens';

type Mode = 'login' | 'register';

/**
 * Turn an auth failure into a message that tells the truth about *which* thing
 * went wrong. The old screen said "Wrong username or password" for everything —
 * including an unreachable computer or a relay reporting it offline — which is the
 * single biggest reason sign-in "looks broken". The client throws a typed
 * `APIError` with a status for anything the gateway (or relay) actually
 * answered; a network-level failure (timeout, abort, refused, DNS) is anything
 * that is *not* an `APIError`.
 */
function describeAuthError(e: unknown, mode: Mode): string {
  if (!(e instanceof APIError)) {
    return "Can't reach your computer. Make sure it's awake with Consistent CEO open and this phone is paired, then try again.";
  }
  if (e.status === 503) {
    return 'Your computer is offline right now — open Consistent CEO on it, then try again.';
  }
  if (e.status === 401 || e.status === 403) {
    return mode === 'register' ? 'Could not create that account.' : 'Wrong username or password.';
  }
  if (mode === 'register' && (e.status === 400 || e.status === 409)) {
    return 'Could not create that account — that username may already be taken.';
  }
  return `Something went wrong (error ${e.status}). Please try again in a moment.`;
}

export default function LoginScreen() {
  const theme = useTheme();
  const router = useRouter();
  const insets = useSafeAreaInsets();

  const [mode, setMode] = useState<Mode>('login');
  const [username, setUsername] = useState('');
  const [password, setPassword] = useState('');
  const [email, setEmail] = useState('');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [codes, setCodes] = useState<string[] | null>(null);
  // Null until we know; true means the app is still on its built-in default URL
  // (the phone's own loopback), so there is no computer to sign in to yet.
  const [needsPairing, setNeedsPairing] = useState<boolean | null>(null);
  // Signing in over plain http beyond the home Wi-Fi deserves a heads-up.
  const [httpNote, setHttpNote] = useState<string | null>(null);

  const deviceLabel = `${Platform.OS === 'ios' ? 'iPhone' : 'Android'} · Consistent CEO`;

  useEffect(() => {
    void isDemoGateway().then(setNeedsPairing);
    void getGatewayUrl().then((url) => setHttpNote(plainHttpNote(url)));
  }, []);

  const done = async () => {
    // Register this device as an authenticator so it can approve other logins.
    try {
      await auth.registerDevice(deviceLabel, Platform.OS);
    } catch {
      // non-fatal
    }
    router.replace('/(tabs)');
  };

  const submit = async () => {
    setError(null);
    if (!username.trim() || !password) {
      setError('Enter a username and password.');
      return;
    }
    setBusy(true);
    try {
      if (mode === 'register') {
        const res = await auth.register(username.trim(), password, {
          email: email.trim() || undefined,
          device: deviceLabel,
        });
        // Show recovery codes once before continuing.
        setCodes(res.recoveryCodes ?? []);
      } else {
        await auth.login(username.trim(), password, deviceLabel);
        await done();
      }
    } catch (e) {
      setError(describeAuthError(e, mode));
    } finally {
      setBusy(false);
    }
  };

  if (codes) {
    return (
      <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top + spacing[6], paddingHorizontal: spacing[6], gap: spacing[4] }}>
        <Txt variant="title">Save your recovery codes</Txt>
        <Txt variant="body" tone="secondary">
          These are shown once. Store them somewhere safe — each lets you recover your account without email.
        </Txt>
        <View style={{ backgroundColor: theme.colors.bgCard, borderRadius: radius.lg, borderWidth: 1, borderColor: theme.colors.border, padding: spacing[4], gap: spacing[2] }}>
          {codes.map((c) => (
            <Txt key={c} weight="semibold" style={{ fontVariant: ['tabular-nums'], letterSpacing: 2 }}>
              {c}
            </Txt>
          ))}
        </View>
        <NativeButton label="I've saved them" fullWidth onPress={done} />
      </View>
    );
  }

  // Avoid flashing the sign-in form before we know whether a computer is paired.
  if (needsPairing === null) {
    return <View style={{ flex: 1, backgroundColor: theme.colors.bg }} />;
  }

  // Not connected to a computer yet: signing in against the phone's own loopback can
  // only ever fail, so send the operator to pair first instead of letting them
  // type credentials into a wall.
  if (needsPairing) {
    return (
      <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top + spacing[8], paddingHorizontal: spacing[6], gap: spacing[4] }}>
        <View style={{ gap: spacing[1] }}>
          <Txt variant="title">Connect to your computer</Txt>
          <Txt variant="body" tone="secondary" style={{ lineHeight: 22 }}>
            This app is a remote control for Consistent CEO running on your computer. Pair with the
            QR code shown in the desktop app before signing in.
          </Txt>
        </View>
        <View style={{ gap: spacing[2] }}>
          <NativeButton label="Pair with your computer" icon="◉" fullWidth onPress={() => router.push('/pair')} />
          <NativeButton
            label="Enter your computer’s address manually"
            variant="ghost"
            fullWidth
            onPress={() => router.push('/settings')}
          />
        </View>
      </View>
    );
  }

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg, paddingTop: insets.top + spacing[8], paddingHorizontal: spacing[6], gap: spacing[4] }}>
      <View style={{ gap: spacing[1] }}>
        <Txt variant="title">{mode === 'login' ? 'Sign in' : 'Create account'}</Txt>
        <Txt variant="body" tone="secondary">
          {mode === 'login' ? 'Welcome back to Consistent CEO.' : 'Username and password — email is optional.'}
        </Txt>
      </View>

      <Field label="Username" value={username} onChange={setUsername} autoCapitalize="none" theme={theme} />
      <Field label="Password" value={password} onChange={setPassword} secure theme={theme} />
      {mode === 'register' ? (
        <Field label="Email (optional)" value={email} onChange={setEmail} autoCapitalize="none" theme={theme} />
      ) : null}

      {error ? (
        <Txt variant="body" style={{ color: theme.colors.danger }}>
          {error}
        </Txt>
      ) : null}

      {httpNote ? (
        <Txt variant="caption" tone="muted" style={{ lineHeight: 17 }}>
          {httpNote}
        </Txt>
      ) : null}

      <View style={{ gap: spacing[2] }}>
        <NativeButton label={mode === 'login' ? 'Sign in' : 'Create account'} fullWidth loading={busy} onPress={submit} />
        <NativeButton
          label={mode === 'login' ? 'Create an account' : 'I already have an account'}
          variant="ghost"
          fullWidth
          onPress={() => {
            setError(null);
            setMode(mode === 'login' ? 'register' : 'login');
          }}
        />
      </View>
    </View>
  );
}

function Field({
  label,
  value,
  onChange,
  secure,
  autoCapitalize,
  theme,
}: {
  label: string;
  value: string;
  onChange: (v: string) => void;
  secure?: boolean;
  autoCapitalize?: 'none' | 'sentences';
  theme: ReturnType<typeof useTheme>;
}) {
  return (
    <View style={{ gap: spacing[1] }}>
      <Txt variant="caption" tone="muted">
        {label}
      </Txt>
      <TextInput
        value={value}
        onChangeText={onChange}
        secureTextEntry={secure}
        autoCapitalize={autoCapitalize ?? 'sentences'}
        autoCorrect={false}
        placeholderTextColor={theme.colors.textMuted}
        style={{
          backgroundColor: theme.colors.bgCard,
          borderRadius: radius.md,
          borderWidth: 1,
          borderColor: theme.colors.border,
          paddingHorizontal: spacing[3],
          paddingVertical: spacing[3],
          color: theme.colors.textPrimary,
          fontSize: 16,
        }}
      />
    </View>
  );
}
