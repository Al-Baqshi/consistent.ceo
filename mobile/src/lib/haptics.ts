import * as Haptics from 'expo-haptics';
import { Platform } from 'react-native';

/**
 * Thin, semantic haptics layer. Never throws — haptics are best-effort and
 * silently no-op on web / unsupported devices.
 */

function safe(fn: () => Promise<void>) {
  if (Platform.OS === 'web') return;
  fn().catch(() => {});
}

export const haptics = {
  /** Light tap — tab changes, chip selection, small toggles. */
  tap() {
    safe(() => Haptics.selectionAsync());
  },
  /** Medium press — primary buttons, opening a workspace. */
  press() {
    safe(() => Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Medium));
  },
  /** Heavy — long-press / entering record mode. */
  heavy() {
    safe(() => Haptics.impactAsync(Haptics.ImpactFeedbackStyle.Heavy));
  },
  /** Success — capture saved, action approved. */
  success() {
    safe(() => Haptics.notificationAsync(Haptics.NotificationFeedbackType.Success));
  },
  /** Warning — needs attention. */
  warning() {
    safe(() => Haptics.notificationAsync(Haptics.NotificationFeedbackType.Warning));
  },
  /** Error — rejected / failed. */
  error() {
    safe(() => Haptics.notificationAsync(Haptics.NotificationFeedbackType.Error));
  },
};
