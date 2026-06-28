// Panel registry: id, label, and a terse forge-glyph used in the rail. Order is
// the nav order, Overview first.

export interface NavItem {
  id: string;
  label: string;
  glyph: string;
}

export const NAV: NavItem[] = [
  { id: 'overview', label: 'Overview', glyph: '◎' },
  { id: 'players', label: 'Players', glyph: '⦿' },
  { id: 'world', label: 'World', glyph: '▦' },
  { id: 'packets', label: 'Packets', glyph: '⇅' },
  { id: 'queues', label: 'Queues', glyph: '≣' },
  { id: 'persistence', label: 'Persistence', glyph: '▭' },
  { id: 'plugins', label: 'Plugins', glyph: '◆' }
];
