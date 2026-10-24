import * as monaco from 'monaco-editor/editor';
import 'monaco-editor/features/register.all';
import 'monaco-editor/languages/definitions/javascript/register';
import EditorWorker from 'monaco-editor/editor/editor.worker.js?worker';
import * as Y from 'yjs';
import { MonacoBinding } from 'y-monaco';
import { Awareness, encodeAwarenessUpdate, applyAwarenessUpdate } from 'y-protocols/awareness';

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };
const doc = new Y.Doc();
const text = doc.getText('code');
const settings = doc.getMap('settings');
const awareness = new Awareness(doc);
const model = monaco.editor.createModel('', 'javascript');
const editor = monaco.editor.create(document.getElementById('editor'), {
  model, automaticLayout: true, autoIndent: 'full', insertSpaces: true,
  tabSize: 4, detectIndentation: false, quickSuggestions: false,
});
const binding = new MonacoBinding(text, model, new Set([editor]), awareness);
const undo = new Y.UndoManager(text, { trackedOrigins: new Set([binding]) });
editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyZ, () => undo.undo());
editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyY, () => undo.redo());
settings.observe(() => {
  const language = settings.get('language');
  if (language) monaco.editor.setModelLanguage(model, language);
  const tabSize = settings.get('tabSize');
  if (tabSize) model.updateOptions({ tabSize, indentSize: tabSize, insertSpaces: true });
});
let serverVector = new Uint8Array([0]);
window.spike = {
  async sync() {
    const response = await fetch('/sync', {
      method: 'POST', headers: { 'Content-Type': 'application/json' },
      signal: AbortSignal.timeout(10000),
      body: JSON.stringify({
        update: Array.from(Y.encodeStateAsUpdate(doc, serverVector)),
        vector: Array.from(Y.encodeStateVector(doc)),
      }),
    });
    if (!response.ok) throw new Error(`sync failed: ${response.status}`);
    const result = await response.json();
    Y.applyUpdate(doc, new Uint8Array(result.update), 'remote');
    serverVector = new Uint8Array(result.vector);
    return result.text;
  },
  insert(value) {
    const position = model.getPositionAt(model.getValueLength());
    undo.stopCapturing();
    editor.executeEdits('spike', [{ range: monaco.Range.fromPositions(position), text: value }]);
  },
  value: () => model.getValue(),
  configure(language, tabSize) { settings.set('language', language); settings.set('tabSize', tabSize); },
  config: () => ({ language: model.getLanguageId(), tabSize: model.getOptions().tabSize }),
  focusEnd() { editor.focus(); editor.setPosition(model.getPositionAt(model.getValueLength())); },
  presence(name, color) {
    awareness.setLocalStateField('user', { name, color });
    editor.setSelection(new monaco.Range(1, 1, 1, 3));
    return Array.from(encodeAwarenessUpdate(awareness, [doc.clientID]));
  },
  receivePresence(update) { applyAwarenessUpdate(awareness, new Uint8Array(update), 'remote'); },
  participants: () => Array.from(awareness.getStates().values()),
  remoteSelections: () => model.getAllDecorations().filter(d => d.options.className?.includes('yRemoteSelection')).length,
  clearUndo: () => undo.clear(),
};
