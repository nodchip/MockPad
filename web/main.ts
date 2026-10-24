import * as monaco from 'monaco-editor/editor';
import 'monaco-editor/features/register.all';
import 'monaco-editor/languages/definitions/register.all';
import EditorWorker from 'monaco-editor/editor/editor.worker.js?worker';
import * as Y from 'yjs';
import { MonacoBinding } from 'y-monaco';
import * as awarenessProtocol from 'y-protocols/awareness';
import * as syncProtocol from 'y-protocols/sync';
import * as encoding from 'lib0/encoding';
import * as decoding from 'lib0/decoding';
import './style.css';

self.MonacoEnvironment = { getWorker: () => new EditorWorker() };
const app = document.querySelector<HTMLDivElement>('#app')!;
const REMOTE = 'remote';
const palette = ['#237b65', '#b15e25', '#6752b8', '#246da4', '#b1446b', '#7b741d'];
const element = <T extends HTMLElement = HTMLElement>(id: string) => document.getElementById(id) as T;
let active: RoomEditor | undefined;

function shell(content: string) {
  app.innerHTML = `<header class="site-header"><a class="brand" href="/"><span class="brand-mark">M</span>MockPad</a><span class="tagline">模擬面接のための共同エディター</span></header>${content}`;
}

function unavailable(message = 'この部屋は終了したか、存在しません。') {
  active?.dispose();
  active = undefined;
  shell('<main class="welcome"><div class="eyebrow">ROOM CLOSED</div><h1>この部屋は利用できません</h1><p id="reason"></p><a class="button primary" href="/">新しい部屋を作る</a></main>');
  element('reason').textContent = message;
}

async function api(path: string, options: RequestInit = {}) {
  return fetch(path, { ...options, cache: 'no-store', signal: AbortSignal.timeout(10000) });
}

async function landing(roomId?: string) {
  shell(`<main class="welcome">
    <div class="eyebrow">${roomId ? 'JOIN A ROOM' : 'CODE. THINK. TOGETHER.'}</div>
    <h1>${roomId ? '面接の部屋に参加する' : '同じコードで、<br>考え方を共有する。'}</h1>
    <p>${roomId ? '表示名を入力して共同編集を始めましょう。' : '面接官と受験者のための、シンプルな共同コードエディター。<br>部屋を作り、URLを共有するだけで始められます。'}</p>
    <form id="entry" class="entry-card"><label for="display-name">表示名</label>
      <input id="display-name" name="name" maxlength="40" required autocomplete="nickname" placeholder="例：山田" autofocus>
      <button class="primary" id="enter" type="submit">${roomId ? '部屋に参加する' : '部屋を作成する'} <span aria-hidden="true">→</span></button>
      <p class="error" id="error" role="alert"></p>
    </form>
    <div class="notes"><span>ログイン不要</span><span>リアルタイム共同編集</span><span>通話・講評はDiscordで</span></div>
    <p class="retention">部屋は全員が退出してから24時間で削除されます。<br>面接官の終了操作やサーバー再起動でも削除されます。</p>
  </main>`);
  if (roomId) {
    try {
      const response = await api(`/api/rooms/${roomId}`);
      if (response.status === 404) { unavailable(); return; }
      if (!response.ok) throw new Error();
    } catch { element('error').textContent = '部屋を確認できません。接続を確認してから参加してください。'; }
  }
  element<HTMLFormElement>('entry').onsubmit = async event => {
    event.preventDefault();
    const name = element<HTMLInputElement>('display-name').value.trim();
    if (!name) { element('error').textContent = '表示名を入力してください。'; return; }
    const button = element<HTMLButtonElement>('enter');
    button.disabled = true;
    element('error').textContent = '';
    try {
      if (!roomId) {
        const response = await api('/api/rooms', { method: 'POST' });
        if (response.status === 429) {
          element('error').textContent = '部屋作成の上限に達しました。しばらく待ってから再度お試しください。';
          button.disabled = false;
          return;
        }
        if (!response.ok) throw new Error();
        roomId = (await response.json()).id as string;
        history.pushState(null, '', `/r/${roomId}`);
      }
      active = new RoomEditor(roomId!, name);
    } catch {
      button.disabled = false;
      element('error').textContent = '接続できませんでした。時間をおいて再度お試しください。';
    }
  };
}

class RoomEditor {
  private doc = new Y.Doc();
  private text = this.doc.getText('code');
  private settings = this.doc.getMap('settings');
  private awareness = new awarenessProtocol.Awareness(this.doc);
  private model: monaco.editor.ITextModel;
  private editor: monaco.editor.IStandaloneCodeEditor;
  private binding: MonacoBinding;
  private undo: Y.UndoManager;
  private socket?: WebSocket;
  private retry?: ReturnType<typeof setTimeout>;
  private attempts = 0;
  private disposed = false;
  private ready = false;
  private styles: HTMLStyleElement;

  constructor(private id: string, name: string) {
    shell(`<main class="workspace">
      <div class="room-heading"><div><div class="eyebrow">INTERVIEW ROOM</div><h1>共同編集ルーム</h1></div><span id="connection" class="connection" role="status">接続中</span></div>
      <div class="toolbar">
        <label>言語<select id="language" aria-label="言語" disabled></select></label>
        <label>タブサイズ<select id="tab-size" aria-label="タブサイズ" disabled><option value="2">2</option><option value="4" selected>4</option><option value="8">8</option></select></label>
        <div class="toolbar-actions"><button id="undo" title="元に戻す（Ctrl/Cmd+Z）" disabled>元に戻す</button><button id="redo" title="やり直す（Ctrl+Y / Cmd+Shift+Z）" disabled>やり直す</button></div>
        <div class="toolbar-end"><button id="invite">参加URLをコピー</button><button id="end-room" class="danger" hidden>部屋を終了</button></div>
      </div>
      <div class="participants-bar"><div id="participants" aria-label="参加者"></div><span id="notice" role="status"></span></div>
      <div id="editor" aria-label="共同コードエディター"></div>
      <footer class="editor-footer"><span>自動同期 · スペースでインデント</span><span>通話・講評はDiscordで</span></footer>
    </main>
    <dialog id="close-dialog"><h2>部屋を終了しますか？</h2><p>全員の接続を終了し、コードを削除します。<br>この操作は取り消せません。</p><p id="close-error" class="error" role="alert"></p><div class="dialog-actions"><button id="cancel-close">キャンセル</button><button id="confirm-close" class="danger solid">終了する</button></div></dialog>`);
    const language = element<HTMLSelectElement>('language');
    const languages = monaco.languages.getLanguages().sort((a, b) => (a.aliases?.[0] ?? a.id).localeCompare(b.aliases?.[0] ?? b.id));
    for (const item of languages) {
      const option = document.createElement('option');
      option.value = item.id;
      option.textContent = item.id === 'plaintext' ? 'プレーンテキスト' : (item.aliases?.[0] ?? item.id);
      language.append(option);
    }
    language.value = 'plaintext';
    this.model = monaco.editor.createModel('', 'plaintext');
    this.editor = monaco.editor.create(element('editor'), {
      model: this.model, automaticLayout: true, readOnly: true, fontSize: 15,
      fontFamily: "'Cascadia Code', 'Consolas', monospace", lineHeight: 25,
      minimap: { enabled: false }, scrollBeyondLastLine: false, padding: { top: 18 },
      insertSpaces: true, tabSize: 4, detectIndentation: false, autoIndent: 'full',
      quickSuggestions: false, wordBasedSuggestions: 'off',
      suggestOnTriggerCharacters: false, contextmenu: true,
    });
    this.binding = new MonacoBinding(this.text, this.model, new Set([this.editor]), this.awareness);
    this.undo = new Y.UndoManager(this.text, { trackedOrigins: new Set([this.binding]) });
    // Monaco's keyboard and context-menu actions also ultimately use model.undo/redo.
    this.model.undo = () => { if (this.ready) this.undo.undo(); };
    this.model.redo = () => { if (this.ready) this.undo.redo(); };
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyZ, () => this.model.undo());
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyCode.KeyY, () => this.model.redo());
    this.editor.addCommand(monaco.KeyMod.CtrlCmd | monaco.KeyMod.Shift | monaco.KeyCode.KeyZ, () => this.model.redo());
    this.editor.addAction({ id: 'mockpad.undo', label: '元に戻す', contextMenuGroupId: '0_undo', contextMenuOrder: 1, precondition: '!editorReadonly', run: () => this.model.undo() });
    this.editor.addAction({ id: 'mockpad.redo', label: 'やり直す', contextMenuGroupId: '0_undo', contextMenuOrder: 2, precondition: '!editorReadonly', run: () => this.model.redo() });
    element('undo').onclick = () => this.model.undo();
    element('redo').onclick = () => this.model.redo();
    for (const event of ['stack-item-added', 'stack-item-popped', 'stack-cleared'] as const) this.undo.on(event, () => this.updateUndo());
    this.doc.on('update', (update: Uint8Array, origin: unknown) => {
      if (origin !== REMOTE) {
        const encoder = encoding.createEncoder();
        encoding.writeVarUint(encoder, 0);
        syncProtocol.writeUpdate(encoder, update);
        this.send(encoding.toUint8Array(encoder));
      }
    });
    this.settings.observe(() => this.applySettings());
    language.onchange = () => { if (this.ready) this.settings.set('language', language.value); };
    element<HTMLSelectElement>('tab-size').onchange = () => {
      if (this.ready) this.settings.set('tabSize', Number(element<HTMLSelectElement>('tab-size').value));
    };
    this.styles = document.createElement('style');
    document.head.append(this.styles);
    this.awareness.on('update', ({ added, updated, removed }: { added: number[]; updated: number[]; removed: number[] }, origin: unknown) => {
      if (origin !== REMOTE) {
        const ids = [...added, ...updated, ...removed].filter(id => id === this.doc.clientID);
        if (ids.length) {
          const encoder = encoding.createEncoder();
          encoding.writeVarUint(encoder, 1);
          encoding.writeVarUint8Array(encoder, awarenessProtocol.encodeAwarenessUpdate(this.awareness, ids));
          this.send(encoding.toUint8Array(encoder));
        }
      }
      this.renderPresence();
    });
    this.awareness.setLocalStateField('user', { name, color: palette[this.doc.clientID % palette.length] });
    element('invite').onclick = async () => {
      try {
        await navigator.clipboard.writeText(`${location.origin}/r/${this.id}`);
        element('notice').textContent = '参加URLをコピーしました';
      } catch { element('notice').textContent = 'URLをコピーできませんでした。アドレスバーのURLを共有してください。'; }
    };
    const dialog = element<HTMLDialogElement>('close-dialog');
    element('end-room').onclick = () => dialog.showModal();
    element('cancel-close').onclick = () => dialog.close();
    element('confirm-close').onclick = async () => {
      const button = element<HTMLButtonElement>('confirm-close');
      button.disabled = true;
      element('close-error').textContent = '';
      try {
        const response = await api(`/api/rooms/${this.id}/close`, { method: 'POST' });
        if (response.ok || response.status === 404) unavailable('部屋を終了しました。コードは削除されました。');
        else throw new Error();
      } catch {
        if (!this.disposed) {
          button.disabled = false;
          element('close-error').textContent = '終了できませんでした。接続と権限を確認して再度お試しください。';
        }
      }
    };
    void this.connect();
  }

  private updateUndo() {
    element<HTMLButtonElement>('undo').disabled = !this.ready || !this.undo.canUndo();
    element<HTMLButtonElement>('redo').disabled = !this.ready || !this.undo.canRedo();
  }

  private setStatus(label: string, ready = false) {
    this.ready = ready;
    element('connection').textContent = label;
    element('connection').classList.toggle('online', ready);
    this.editor.updateOptions({ readOnly: !ready });
    element<HTMLSelectElement>('language').disabled = !ready;
    element<HTMLSelectElement>('tab-size').disabled = !ready;
    this.updateUndo();
  }

  private send(data: Uint8Array) {
    if (data.byteLength > 256 * 1024) {
      // Finish the Yjs/Monaco transaction before disposing its model and observers.
      queueMicrotask(() => {
        if (!this.disposed) unavailable('一度に送れる編集量の上限を超えました。新しい部屋で、入力を分けてお試しください。');
      });
      return;
    }
    if (this.socket?.readyState === WebSocket.OPEN) this.socket.send(data);
  }

  private async connect() {
    if (this.disposed) return;
    this.setStatus(this.attempts ? '再接続中' : '接続中');
    try {
      const response = await api(`/api/rooms/${this.id}`);
      if (this.disposed) return;
      if (response.status === 404) { unavailable(); return; }
      if (!response.ok) throw new Error();
      const metadata = await response.json();
      if (this.disposed) return;
      element('end-room').hidden = !metadata.isHost;
      const socket = new WebSocket(`${location.protocol === 'https:' ? 'wss:' : 'ws:'}//${location.host}/api/rooms/${this.id}/ws`);
      this.socket = socket;
      socket.binaryType = 'arraybuffer';
      socket.onopen = () => {
        if (this.disposed || this.socket !== socket) return;
        this.setStatus('同期中');
        const encoder = encoding.createEncoder();
        encoding.writeVarUint(encoder, 0);
        syncProtocol.writeSyncStep1(encoder, this.doc);
        this.send(encoding.toUint8Array(encoder));
        this.awareness.setLocalStateField('user', this.awareness.getLocalState()?.user);
      };
      socket.onmessage = event => {
        if (this.disposed || this.socket !== socket) return;
        try {
          const decoder = decoding.createDecoder(new Uint8Array(event.data));
          while (decoding.hasContent(decoder)) {
            const type = decoding.readVarUint(decoder);
            if (type === 0) {
              const encoder = encoding.createEncoder();
              encoding.writeVarUint(encoder, 0);
              const syncType = syncProtocol.readSyncMessage(decoder, encoder, this.doc, REMOTE);
              if (encoding.length(encoder) > 1) this.send(encoding.toUint8Array(encoder));
              if (syncType === syncProtocol.messageYjsSyncStep2) {
                this.attempts = 0;
                this.applySettings();
                this.setStatus('接続済み', true);
              }
            } else if (type === 1) {
              awarenessProtocol.applyAwarenessUpdate(this.awareness, decoding.readVarUint8Array(decoder), REMOTE);
            } else throw new Error('Unsupported sync message');
          }
        } catch { socket.close(); }
      };
      socket.onclose = event => {
        if (this.disposed || this.socket !== socket) return;
        if (event.code === 4000) { unavailable('部屋を終了しました。コードは削除されました。'); return; }
        if (event.code === 4002 || event.code === 1009) {
          unavailable('入力内容または通信量が上限を超えたため接続を終了しました。編集を続けるには新しい部屋を作成してください。');
          return;
        }
        this.scheduleRetry();
      };
      socket.onerror = () => socket.close();
    } catch { if (!this.disposed) this.scheduleRetry(); }
  }

  private scheduleRetry() {
    this.setStatus('再接続中');
    const remote = [...this.awareness.getStates().keys()].filter(id => id !== this.doc.clientID);
    awarenessProtocol.removeAwarenessStates(this.awareness, remote, REMOTE);
    clearTimeout(this.retry);
    const delay = Math.min(1000 * 2 ** Math.min(this.attempts++, 4), 10000);
    this.retry = setTimeout(() => void this.connect(), delay);
  }

  private applySettings() {
    const language = this.settings.get('language');
    if (typeof language === 'string' && [...element<HTMLSelectElement>('language').options].some(option => option.value === language)) {
      monaco.editor.setModelLanguage(this.model, language);
      element<HTMLSelectElement>('language').value = language;
    }
    const size = this.settings.get('tabSize');
    if (size === 2 || size === 4 || size === 8) {
      this.model.updateOptions({ tabSize: size, indentSize: size, insertSpaces: true });
      element<HTMLSelectElement>('tab-size').value = String(size);
    }
  }

  private renderPresence() {
    const list = element('participants');
    list.replaceChildren();
    const rules: string[] = [];
    const used = new Set<string>();
    for (const [id, state] of [...this.awareness.getStates()].sort((a, b) => a[0] - b[0])) {
      if (!state.user || typeof state.user !== 'object') continue;
      const name = typeof state.user.name === 'string' ? state.user.name.slice(0, 40) : '参加者';
      const preferred = palette.includes(state.user.color) ? state.user.color as string : palette[0];
      const color = used.has(preferred) ? (palette.find(c => !used.has(c)) ?? preferred) : preferred;
      used.add(color);
      const badge = document.createElement('span');
      badge.className = 'participant';
      badge.style.setProperty('--participant-color', color);
      badge.textContent = `${name}${id === this.doc.clientID ? '（自分）' : ''}`;
      list.append(badge);
      rules.push(`.yRemoteSelection-${id}{background:${color}25}.yRemoteSelectionHead-${id}{border-color:${color}}.yRemoteSelectionHead-${id}::after{background:${color};content:${JSON.stringify(name)}}`);
    }
    this.styles.textContent = rules.join('\n');
  }

  dispose() {
    if (this.disposed) return;
    this.disposed = true;
    clearTimeout(this.retry);
    this.awareness.setLocalState(null);
    this.socket?.close();
    this.binding.destroy();
    this.undo.destroy();
    this.awareness.destroy();
    this.editor.dispose();
    this.model.dispose();
    this.doc.destroy();
    this.styles.remove();
  }
}

window.addEventListener('pagehide', () => active?.dispose());
window.addEventListener('popstate', () => location.reload());
const match = /^\/r\/([a-f0-9]{32})\/?$/.exec(location.pathname);
if (location.pathname === '/') void landing();
else if (match) void landing(match[1]);
else unavailable();
