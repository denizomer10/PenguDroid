import type { AppEvidenceSeed, DocumentLinkSeed, FaqSeed, MissingCallSeed, ReleaseSeed, ScenarioSeed, SiteStatSeed } from './types';
import { auditedScenarios, missingJavaCalls } from './compatibility.generated';
import { documentLinks } from './docs';
import { appEvidence } from './evidence';
import { faqs, releases } from './faq';
import { siteStats } from './stats';

export const compatibilityScenarios: ScenarioSeed[] = auditedScenarios;

export const missingCalls: MissingCallSeed[] = missingJavaCalls.map((signature, i) => ({
  signature,
  sortOrder: i + 1,
}));

export const appEvidenceRows: AppEvidenceSeed[] = appEvidence;
export const documentRows: DocumentLinkSeed[] = documentLinks;
export const faqRows: FaqSeed[] = faqs;
export const releaseRows: ReleaseSeed[] = releases;
export const statRows: SiteStatSeed[] = siteStats;
