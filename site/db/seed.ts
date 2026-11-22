import { db, SiteStat, CompatibilityScenario, MissingCall, AppEvidence, DocumentLink, Faq, Release } from 'astro:db';
import {
  appEvidenceRows,
  compatibilityScenarios,
  documentRows,
  faqRows,
  missingCalls,
  releaseRows,
  statRows,
} from '../src/data/index';

export default async function seed() {
  await db.insert(SiteStat).values(statRows);
  await db.insert(CompatibilityScenario).values(
    compatibilityScenarios.map((scenario) => ({
      id: scenario.sortOrder,
      name: scenario.name,
      status: scenario.status,
      evidence: scenario.evidence,
      sortOrder: scenario.sortOrder,
    })),
  );
  await db.insert(MissingCall).values(
    missingCalls.map((call) => ({
      id: call.sortOrder,
      signature: call.signature,
      sortOrder: call.sortOrder,
    })),
  );
  await db.insert(AppEvidence).values(appEvidenceRows);
  await db.insert(DocumentLink).values(documentRows);
  await db.insert(Faq).values(faqRows.map((faq, i) => ({ id: i + 1, ...faq })));
  await db.insert(Release).values(releaseRows);

  console.log(
    `[seed] ${statRows.length} stats, ${compatibilityScenarios.length} scenarios, ${missingCalls.length} missing calls, ` +
      `${appEvidenceRows.length} evidence entries, ${documentRows.length} documents, ${faqRows.length} FAQs, ${releaseRows.length} releases`,
  );
}
