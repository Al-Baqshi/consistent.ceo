import { useWindowDimensions } from 'react-native';

export interface Responsive {
  width: number;
  height: number;
  /** True on iPad / large landscape — switch to the sidebar layout. */
  isWide: boolean;
  isLandscape: boolean;
}

/** Breakpoint above which we use the iPad three-pane sidebar layout. */
export const WIDE_BREAKPOINT = 900;

export function useResponsive(): Responsive {
  const { width, height } = useWindowDimensions();
  return {
    width,
    height,
    isWide: width >= WIDE_BREAKPOINT,
    isLandscape: width > height,
  };
}
