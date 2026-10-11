/**
 * Shared tab definitions used by the phone bottom bar, the iPad sidebar and
 * the router's tab titles.
 *
 * Labels follow the desktop sidebar (`sveltfront/src/lib/components/
 * Sidebar.svelte`) wherever the concept is the same: Chat, Entities, Vault,
 * Today. Calendar and Approvals have no desktop page of their own and keep
 * their plain names. Route names are unchanged (`brain`, `review`, `control`)
 * so deep links and notification taps keep working.
 */
export interface TabDef {
  /** Expo Router route name inside the (tabs) group. */
  name: string;
  href: string;
  label: string;
  icon: string;
}

export const TABS: TabDef[] = [
  { name: 'index', href: '/', label: 'Chat', icon: '◉' },
  { name: 'brain', href: '/brain', label: 'Entities', icon: '✷' },
  { name: 'vault', href: '/vault', label: 'Vault', icon: '⚷' },
  { name: 'calendar', href: '/calendar', label: 'Calendar', icon: '▦' },
  { name: 'control', href: '/control', label: 'Approvals', icon: '✓' },
  { name: 'review', href: '/review', label: 'Today', icon: '❑' },
];
