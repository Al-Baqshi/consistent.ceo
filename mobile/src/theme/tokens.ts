/**
 * Consistent CEO — design tokens v2.
 *
 * "Quiet surfaces, one copper accent." The dark surfaces are the desktop's
 * own warm brown-blacks (`sveltfront/src/app.css`: `#0a0908` / `#0e0c0a` /
 * `#16130f`, text `#f2ede4`), so the phone and the desktop read as one
 * product — and the launch splash (`#0a0908`) no longer jumps to a cooler
 * neutral when the first screen paints. Light is clean paper, not beige.
 *
 * Change values here → the whole app updates. Consume via `useTheme()`.
 */

// ── Raw brand palette (theme-independent) ──────────────────────────────
export const palette = {
  // Neutrals — dark (desktop: --color-bg-base / surface / card / card-hover)
  ink: '#0a0908',
  obsidian: '#0e0c0a',
  charcoal: '#16130f',
  graphite: '#1e1a15',
  // Neutrals — light
  paper: '#f6f5f3',
  snow: '#ffffff',
  // Text neutrals (desktop: --color-text-primary / secondary / muted)
  ivory: '#f2ede4',
  mist: '#b3aa9b',
  faint: '#7d7568',
  // Brand accent
  copper: '#c98a4b',
  copperDeep: '#a5652f',
  bronze: '#8a5a2e',
  // Semantic / secondary (kept muted — no more amber wash)
  amber: '#c9a35b',
  champagne: '#d8c39a',
  moss: '#7d8a68',
  smoke: '#8d7ba6',
  slate: '#7c8ba1',
  failed: '#c96a4b',
  white: '#ffffff',
} as const;

// ── Entity type accent map (Brain map + workspace) ─────────────────────
export type EntityType =
  | 'project'
  | 'person'
  | 'company'
  | 'file'
  | 'mission'
  | 'memory'
  | 'agent';

export const entityAccent: Record<EntityType, string> = {
  project: '#c98a4b', // copper
  company: '#7c8ba1', // slate blue
  person: '#c78f6d', // warm tan
  file: '#8b8781', // neutral
  mission: '#b5674a', // terracotta
  memory: '#8d7ba6', // muted violet
  agent: '#7d8a68', // moss
};

/**
 * The colour an entity is drawn in: the operator's own pick (`Entity.colour`,
 * which the desktop already uses to tint its chat) when set, else the accent
 * for its type.
 */
export function entityColor(e: { type: EntityType; colour?: string }): string {
  return e.colour ?? entityAccent[e.type];
}

/**
 * The Entities map is a night sky in both themes — a deliberate, always-dark
 * surface like a photo viewer — so text and chrome that sit directly on it use
 * these instead of the theme's. Warm-aligned with the dark theme so the map
 * reads as part of the same product.
 */
export const sky = {
  // Matches the sky shader's own base so the map never shows a seam.
  bg: '#06070c',
  text: '#f2ede4',
  textSecondary: '#b3aa9b',
  textMuted: '#8a8276',
  overlay: 'rgba(14, 12, 10, 0.80)',
  overlayStrong: 'rgba(22, 19, 15, 0.95)',
  overlayBorder: 'rgba(242, 237, 228, 0.12)',
  control: 'rgba(242, 237, 228, 0.08)',
  accentText: '#e4b98a',
} as const;

export const entityGlyph: Record<EntityType, string> = {
  project: '◆',
  company: '⬡',
  person: '●',
  file: '▢',
  mission: '➤',
  memory: '✦',
  agent: '⬢',
};

// ── Semantic color schemes ─────────────────────────────────────────────
export interface ColorScheme {
  bg: string;
  bgSurface: string;
  bgCard: string;
  bgCardHover: string;
  bgSelected: string;
  bgOverlay: string;

  border: string;
  borderSubtle: string;
  borderFocus: string;

  textPrimary: string;
  textSecondary: string;
  textMuted: string;
  textInverse: string;
  white: string;

  accent: string;
  accentAlt: string;
  accentBronze: string;
  success: string;
  warning: string;
  danger: string;
  info: string;

  // translucent surfaces
  glass: string;
  glassStrong: string;
  sunken: string;

  // chat
  userBubbleFrom: string;
  userBubbleTo: string;
  userBubbleText: string;
  assistantBubble: string;
  assistantBorder: string;
}

const darkColors: ColorScheme = {
  bg: palette.ink,
  bgSurface: palette.obsidian,
  bgCard: palette.charcoal,
  bgCardHover: palette.graphite,
  bgSelected: 'rgba(201, 138, 75, 0.12)',
  bgOverlay: 'rgba(10, 9, 8, 0.78)',

  border: 'rgba(242, 237, 228, 0.09)',
  borderSubtle: 'rgba(242, 237, 228, 0.05)',
  borderFocus: 'rgba(201, 138, 75, 0.45)',

  textPrimary: palette.ivory,
  textSecondary: palette.mist,
  textMuted: palette.faint,
  textInverse: palette.ink,
  white: palette.white,

  accent: palette.copper,
  accentAlt: palette.copperDeep,
  accentBronze: palette.bronze,
  success: palette.moss,
  warning: palette.amber,
  danger: palette.failed,
  info: palette.slate,

  glass: 'rgba(22, 19, 15, 0.72)',
  glassStrong: 'rgba(22, 19, 15, 0.92)',
  sunken: 'rgba(10, 9, 8, 0.9)',

  userBubbleFrom: palette.copper,
  userBubbleTo: palette.bronze,
  userBubbleText: palette.white,
  assistantBubble: palette.charcoal,
  assistantBorder: 'rgba(242, 237, 228, 0.08)',
};

const lightColors: ColorScheme = {
  bg: palette.paper,
  bgSurface: palette.snow,
  bgCard: palette.snow,
  bgCardHover: '#f0eeeb',
  bgSelected: 'rgba(176, 106, 53, 0.10)',
  bgOverlay: 'rgba(20, 19, 22, 0.5)',

  border: 'rgba(24, 23, 28, 0.10)',
  borderSubtle: 'rgba(24, 23, 28, 0.06)',
  borderFocus: 'rgba(176, 106, 53, 0.45)',

  textPrimary: '#1a1918',
  textSecondary: '#55514c',
  textMuted: '#8b8781',
  textInverse: '#ffffff',
  white: palette.white,

  accent: '#b06a35',
  accentAlt: '#8f5527',
  accentBronze: palette.bronze,
  success: '#5f7050',
  warning: '#a8823d',
  danger: '#b5533c',
  info: '#5b6b7d',

  glass: 'rgba(255, 255, 255, 0.82)',
  glassStrong: 'rgba(255, 255, 255, 0.96)',
  sunken: 'rgba(24, 23, 28, 0.04)',

  userBubbleFrom: palette.copper,
  userBubbleTo: palette.bronze,
  userBubbleText: palette.white,
  assistantBubble: palette.snow,
  assistantBorder: 'rgba(24, 23, 28, 0.10)',
};

// ── Static scales (identical across themes) ────────────────────────────
export const spacing = {
  0: 0,
  1: 4,
  2: 8,
  3: 12,
  4: 16,
  5: 20,
  6: 24,
  8: 32,
  10: 40,
  12: 48,
  16: 64,
} as const;

export const radius = {
  sm: 6,
  md: 10,
  lg: 14,
  xl: 18,
  '2xl': 24,
  full: 9999,
} as const;

// Mobile-first type scale — larger than the dense web scale.
export const fontSize = {
  '2xs': 11,
  xs: 12,
  sm: 13,
  base: 15,
  md: 16,
  lg: 18,
  xl: 22,
  '2xl': 28,
  '3xl': 34,
} as const;

export const fontWeight = {
  normal: '400',
  medium: '500',
  semibold: '600',
  bold: '700',
} as const;

export const duration = {
  fast: 120,
  base: 200,
  slow: 320,
} as const;

// Minimum comfortable touch target (Apple HIG / Material).
export const HIT_SLOP = { top: 8, bottom: 8, left: 8, right: 8 };
export const MIN_TOUCH = 44;

export const gradients = {
  primaryButton: [palette.copper, palette.bronze] as const,
  hero: ['rgba(201, 138, 75, 0.14)', palette.ink] as const,
  coreGlow: [
    'rgba(201, 138, 75, 0.5)',
    'rgba(138, 90, 46, 0.2)',
    'transparent',
  ] as const,
  text: [palette.ivory, palette.copper, palette.bronze] as const,
};

export type ThemeMode = 'dark' | 'light';

export interface Theme {
  mode: ThemeMode;
  colors: ColorScheme;
  spacing: typeof spacing;
  radius: typeof radius;
  fontSize: typeof fontSize;
  fontWeight: typeof fontWeight;
  duration: typeof duration;
  gradients: typeof gradients;
}

export function buildTheme(mode: ThemeMode): Theme {
  return {
    mode,
    colors: mode === 'dark' ? darkColors : lightColors,
    spacing,
    radius,
    fontSize,
    fontWeight,
    duration,
    gradients,
  };
}
