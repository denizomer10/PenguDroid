import type { SiteStatSeed } from './types';
import { compatibilitySource } from './compatibility.generated';

const { jni, counts } = compatibilitySource;

export const siteStats: SiteStatSeed[] = [
  {
    key: 'jni-slots',
    label: 'JNI function slots',
    value: String(jni.functionSlots),
    note: 'four reserved entries excluded',
    sortOrder: 1,
  },
  {
    key: 'jni-wired',
    label: 'Wired handlers',
    value: String(jni.wired),
    note: 'wiring, not semantic conformance',
    sortOrder: 2,
  },
  {
    key: 'jni-placeholders',
    label: 'Placeholder handlers',
    value: String(jni.placeholders),
    note: 'may be partial or no-op',
    sortOrder: 3,
  },
  {
    key: 'scenarios',
    label: 'Audited scenarios',
    value: String(counts.passed + counts.partial + counts.missing),
    note: `${counts.passed} passed · ${counts.partial} partial · ${counts.missing} missing`,
    sortOrder: 4,
  },
  {
    key: 'missing-calls',
    label: 'Observed missing signatures',
    value: '71',
    note: 'lower bound from traced calls',
    sortOrder: 5,
  },
  {
    key: 'missing-slots',
    label: 'Missing JNI slots',
    value: '0',
    note: 'observed in the audited traces',
    sortOrder: 6,
  },
  {
    key: 'android-target',
    label: 'Runtime target',
    value: 'API 37',
    note: 'Android 17 · x86_64 Linux',
    sortOrder: 7,
  },
];

export const featuredStatKeys = ['jni-slots', 'jni-wired', 'scenarios', 'missing-calls'];
