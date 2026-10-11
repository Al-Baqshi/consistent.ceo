import { useFocusEffect } from 'expo-router';
import { useCallback, useState } from 'react';

import { isDemoGateway } from '@/api/client';

/**
 * Whether the app is in demo mode — nothing paired, still on the built-in
 * default address. `null` until known, so a screen can avoid flashing its
 * demo panel (or its real one) for a frame. Re-read on every focus: pairing
 * happens in a modal on top of the screen that asks.
 */
export function useDemoMode(): boolean | null {
  const [demo, setDemo] = useState<boolean | null>(null);
  useFocusEffect(
    useCallback(() => {
      let live = true;
      void isDemoGateway().then((d) => {
        if (live) setDemo(d);
      });
      return () => {
        live = false;
      };
    }, []),
  );
  return demo;
}
