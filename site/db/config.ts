import { defineDb, defineTable, column } from 'astro:db';

const SiteStat = defineTable({
  columns: {
    key: column.text({ primaryKey: true }),
    label: column.text(),
    value: column.text(),
    note: column.text(),
    sortOrder: column.number(),
  },
});

const CompatibilityScenario = defineTable({
  columns: {
    id: column.number({ primaryKey: true }),
    name: column.text(),
    status: column.text(),
    evidence: column.text(),
    sortOrder: column.number(),
  },
});

const MissingCall = defineTable({
  columns: {
    id: column.number({ primaryKey: true }),
    signature: column.text(),
    sortOrder: column.number(),
  },
});

const AppEvidence = defineTable({
  columns: {
    slug: column.text({ primaryKey: true }),
    app: column.text(),
    version: column.text(),
    headline: column.text(),
    summary: column.text(),
    caveat: column.text(),
    imageFile: column.text(),
    imageAlt: column.text(),
    pathLabel: column.text(),
    docsHref: column.text(),
    sortOrder: column.number(),
  },
});

const DocumentLink = defineTable({
  columns: {
    slug: column.text({ primaryKey: true }),
    title: column.text(),
    category: column.text(),
    summary: column.text(),
    href: column.text(),
    sortOrder: column.number(),
  },
});

const Faq = defineTable({
  columns: {
    id: column.number({ primaryKey: true }),
    question: column.text(),
    answer: column.text(),
    sortOrder: column.number(),
  },
});

const Release = defineTable({
  columns: {
    version: column.text({ primaryKey: true }),
    name: column.text(),
    status: column.text(),
    summary: column.text(),
    date: column.text(),
    href: column.text(),
    sortOrder: column.number(),
  },
});

export default defineDb({
  tables: {
    SiteStat,
    CompatibilityScenario,
    MissingCall,
    AppEvidence,
    DocumentLink,
    Faq,
    Release,
  },
});
