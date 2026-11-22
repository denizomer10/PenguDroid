import { db } from 'astro:db';
import {
  AppEvidence,
  CompatibilityScenario,
  DocumentLink,
  Faq,
  MissingCall,
  Release,
  SiteStat,
} from 'astro:db';

// Single data-access module. Everything else in the site imports these helpers,
// so swapping Astro DB for another store means editing this file only.
export { db };

export async function getStats() {
  return db.select().from(SiteStat).orderBy(SiteStat.sortOrder);
}

export async function getFeaturedStats(keys: string[]) {
  const stats = await getStats();
  return keys.flatMap((key) => stats.filter((stat) => stat.key === key));
}

export async function getScenarios() {
  return db.select().from(CompatibilityScenario).orderBy(CompatibilityScenario.sortOrder);
}

export async function getMissingCalls() {
  return db.select().from(MissingCall).orderBy(MissingCall.sortOrder);
}

export async function getEvidence() {
  return db.select().from(AppEvidence).orderBy(AppEvidence.sortOrder);
}

export async function getDocuments() {
  return db.select().from(DocumentLink).orderBy(DocumentLink.sortOrder);
}

export async function getFaqs() {
  return db.select().from(Faq).orderBy(Faq.sortOrder);
}

export async function getReleases() {
  return db.select().from(Release).orderBy(Release.sortOrder);
}

export async function getLatestRelease() {
  const releases = await getReleases();
  return releases[0];
}
