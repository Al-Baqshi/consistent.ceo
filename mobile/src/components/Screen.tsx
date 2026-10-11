import React from 'react';
import {
  RefreshControl,
  ScrollView,
  StyleProp,
  View,
  ViewStyle,
} from 'react-native';
import { Edge, SafeAreaView, useSafeAreaInsets } from 'react-native-safe-area-context';

import { useResponsive } from '@/lib/useResponsive';
import { useTheme } from '@/theme/ThemeProvider';
import { spacing } from '@/theme/tokens';

import { ProfileButton } from './nav/ProfileButton';
import { Txt } from './Txt';

/** Height of the floating phone tab bar — content must clear it. */
export const TAB_BAR_CLEARANCE = 76;

interface Props {
  children: React.ReactNode;
  /** Wrap content in a ScrollView. */
  scroll?: boolean;
  onRefresh?: () => void | Promise<void>;
  refreshing?: boolean;
  title?: string;
  subtitle?: string;
  header?: React.ReactNode;
  /** Extra element pinned to the top-right of the header row. */
  headerRight?: React.ReactNode;
  /** Show the account avatar (You → vault, settings). On by default. */
  showProfile?: boolean;
  contentStyle?: StyleProp<ViewStyle>;
  edges?: Edge[];
  padded?: boolean;
}

/**
 * Standard screen wrapper: safe-area, warm ambient background, optional
 * pull-to-refresh and a consistent header.
 */
export function Screen({
  children,
  scroll = false,
  onRefresh,
  refreshing = false,
  title,
  subtitle,
  header,
  headerRight,
  showProfile = true,
  contentStyle,
  edges = ['top'],
  padded = true,
}: Props) {
  const theme = useTheme();
  const insets = useSafeAreaInsets();
  const { isWide } = useResponsive();
  const pad = padded ? { paddingHorizontal: spacing[5] } : null;
  // Clear the floating tab bar on phones (the iPad sidebar needs no bottom inset).
  const bottomPad = isWide ? spacing[8] : insets.bottom + TAB_BAR_CLEARANCE;

  const headerBlock =
    header ??
    (title ? (
      <View
        style={{
          flexDirection: 'row',
          alignItems: 'flex-end',
          justifyContent: 'space-between',
          paddingTop: spacing[2],
          paddingBottom: spacing[4],
        }}
      >
        <View style={{ flex: 1 }}>
          <Txt variant="title">{title}</Txt>
          {subtitle ? (
            <Txt variant="caption" tone="muted" style={{ marginTop: 4 }}>
              {subtitle}
            </Txt>
          ) : null}
        </View>
        <View style={{ flexDirection: 'row', alignItems: 'center', gap: spacing[2] }}>
          {headerRight}
          {/* Account, vault and settings — reachable from every tab. */}
          {showProfile ? <ProfileButton /> : null}
        </View>
      </View>
    ) : null);

  const inner = (
    <>
      {headerBlock ? <View style={pad}>{headerBlock}</View> : null}
      <View style={[{ flex: scroll ? 0 : 1 }, pad, contentStyle]}>{children}</View>
    </>
  );

  return (
    <View style={{ flex: 1, backgroundColor: theme.colors.bg }}>
      {/* Whisper of copper at the top — ambience, not paint. */}
      <View
        pointerEvents="none"
        style={{
          position: 'absolute',
          top: -140,
          left: -60,
          right: -60,
          height: 300,
          backgroundColor: theme.colors.accent,
          opacity: theme.mode === 'dark' ? 0.04 : 0.03,
          borderRadius: 400,
        }}
      />
      <SafeAreaView style={{ flex: 1 }} edges={edges}>
        {scroll ? (
          <ScrollView
            style={{ flex: 1 }}
            contentContainerStyle={{ paddingBottom: bottomPad }}
            showsVerticalScrollIndicator={false}
            keyboardDismissMode="interactive"
            refreshControl={
              onRefresh ? (
                <RefreshControl
                  refreshing={refreshing}
                  onRefresh={onRefresh}
                  tintColor={theme.colors.accent}
                  colors={[theme.colors.accent]}
                />
              ) : undefined
            }
          >
            {inner}
          </ScrollView>
        ) : (
          inner
        )}
      </SafeAreaView>
    </View>
  );
}
