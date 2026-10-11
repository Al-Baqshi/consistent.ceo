import React from 'react';
import { Platform, StyleProp, Text, TextProps, TextStyle } from 'react-native';

import { useTheme } from '@/theme/ThemeProvider';
import { fontSize, fontWeight } from '@/theme/tokens';

type Variant =
  | 'display'
  | 'title'
  | 'heading'
  | 'body'
  | 'label'
  | 'caption'
  | 'mono';

type Tone = 'primary' | 'secondary' | 'muted' | 'accent' | 'inverse' | 'danger' | 'success';

interface Props extends TextProps {
  variant?: Variant;
  tone?: Tone;
  weight?: keyof typeof fontWeight;
  center?: boolean;
  style?: StyleProp<TextStyle>;
}

const variantStyle: Record<Variant, TextStyle> = {
  display: { fontSize: fontSize['3xl'], fontWeight: fontWeight.bold, letterSpacing: -0.5 },
  title: { fontSize: fontSize['2xl'], fontWeight: fontWeight.bold, letterSpacing: -0.4 },
  heading: { fontSize: fontSize.lg, fontWeight: fontWeight.semibold, letterSpacing: -0.2 },
  body: { fontSize: fontSize.md, fontWeight: fontWeight.normal },
  label: { fontSize: fontSize.sm, fontWeight: fontWeight.semibold },
  caption: { fontSize: fontSize.xs, fontWeight: fontWeight.medium },
  mono: { fontSize: fontSize.sm, fontWeight: fontWeight.medium },
};

/** Themed text primitive — the single source of typographic truth. */
export function Txt({
  variant = 'body',
  tone = 'primary',
  weight,
  center,
  style,
  ...rest
}: Props) {
  const theme = useTheme();
  const toneColor: Record<Tone, string> = {
    primary: theme.colors.textPrimary,
    secondary: theme.colors.textSecondary,
    muted: theme.colors.textMuted,
    accent: theme.colors.accent,
    inverse: theme.colors.textInverse,
    danger: theme.colors.danger,
    success: theme.colors.success,
  };
  return (
    <Text
      {...rest}
      style={[
        variantStyle[variant],
        { color: toneColor[tone] },
        variant === 'mono' && { fontFamily: fontFamilyMono() },
        weight ? { fontWeight: fontWeight[weight] } : null,
        center ? { textAlign: 'center' } : null,
        style,
      ]}
    />
  );
}

function fontFamilyMono() {
  return Platform.select({ ios: 'Menlo', android: 'monospace', default: 'monospace' });
}
