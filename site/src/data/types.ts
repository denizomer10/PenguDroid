export type ScenarioStatus = 'passed' | 'partial' | 'missing';

export interface ScenarioSeed {
  name: string;
  status: ScenarioStatus;
  evidence: string;
  sortOrder: number;
}

export interface SiteStatSeed {
  key: string;
  label: string;
  value: string;
  note: string;
  sortOrder: number;
}

export interface MissingCallSeed {
  signature: string;
  sortOrder: number;
}

export interface AppEvidenceSeed {
  slug: string;
  app: string;
  version: string;
  headline: string;
  summary: string;
  caveat: string;
  imageFile: string;
  imageAlt: string;
  pathLabel: string;
  docsHref: string;
  sortOrder: number;
}

export interface DocumentLinkSeed {
  title: string;
  slug: string;
  category: string;
  summary: string;
  href: string;
  sortOrder: number;
}

export interface FaqSeed {
  question: string;
  answer: string;
  sortOrder: number;
}

export interface ReleaseSeed {
  version: string;
  name: string;
  status: string;
  summary: string;
  date: string;
  href: string;
  sortOrder: number;
}
