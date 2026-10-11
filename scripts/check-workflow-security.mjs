import { readdir, readFile } from 'node:fs/promises';
import { join } from 'node:path';

const workflowDirectory = '.github/workflows';
const workflowNames = (await readdir(workflowDirectory)).filter((name) => name.endsWith('.yml') || name.endsWith('.yaml'));
const failures = [];

for (const name of workflowNames) {
  const contents = await readFile(join(workflowDirectory, name), 'utf8');
  for (const [, reference] of contents.matchAll(/^\s*-\s+uses:\s*([^\s#]+)/gm)) {
    const [repository, revision, ...extra] = reference.split('@');
    if (!repository.includes('/') || !/^[a-f0-9]{40}$/.test(revision ?? '') || extra.length > 0) {
      failures.push(`${name}: la acción externa ${reference} debe fijarse a un SHA completo de 40 caracteres`);
    }
  }
}

const releaseWorkflow = await readFile(join(workflowDirectory, 'launcher-release.yml'), 'utf8');
if (!/^permissions:\n  contents: read$/m.test(releaseWorkflow)) {
  failures.push('launcher-release.yml: los permisos globales deben ser de solo lectura');
}
if (!/^  publish:\n    needs: build\n    runs-on: ubuntu-latest\n    permissions:\n      contents: write$/m.test(releaseWorkflow)) {
  failures.push('launcher-release.yml: solo el job publish debe tener contents: write');
}
if (/^  build:\n(?:(?!^  [a-z-]+:).)*?^    permissions:\n      contents: write/m.test(releaseWorkflow)) {
  failures.push('launcher-release.yml: el job build no debe recibir permiso de escritura');
}

if (failures.length > 0) {
  console.error(`Revisión de seguridad de workflows fallida:\n- ${failures.join('\n- ')}`);
  process.exitCode = 1;
} else {
  console.log(`Workflow security passed: ${workflowNames.length} workflows, acciones fijadas a SHA y permisos de release mínimos.`);
}
