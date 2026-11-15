import { pathToFileURL } from 'node:url';
import { readFileSync } from 'node:fs';
const { parseFrontmatter } = await import(pathToFileURL(`${process.argv[2]}/packages/core/src/skills/SkillLoader.ts`).href);
const { frontmatter, body } = parseFrontmatter(readFileSync(new URL('../fixtures/parity-skill.md', import.meta.url), 'utf8'));
console.log('PARITY_RESULT:' + JSON.stringify({ ...frontmatter, content: body }));
