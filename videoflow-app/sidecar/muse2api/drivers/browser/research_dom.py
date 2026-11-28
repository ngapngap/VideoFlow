"""Read-only Muse research-turn capture scripts; no body-text or network access."""

from __future__ import annotations

import json

_CAPTURE = r"""(function(input) {
  const result = (code, extra = {}) => ({pending: false, error: code, ...extra});
  const {owner_id, op_id, stage} = input;
  const begin = `VFCTX BEGIN ${op_id} ${stage}`;
  const end = `VFCTX END ${op_id} ${stage}`;
  const route = location.pathname.match(/(?:^|\/)thread\/([^/?#]+)(?:\/|$)/);
  if (!route || route[1] === 'new') return {pending: true, reason: 'thread-not-ready'};
  const thread_id = decodeURIComponent(route[1]);
  const messages = [...document.querySelectorAll('[data-message-id][data-message-role]')];
  const role = e => e.getAttribute('data-message-role');
  const id = e => e.getAttribute('data-message-id');
  const turn = e => e.getAttribute('data-message-turn-id');
  const users = messages.filter(e => role(e) === 'user' &&
    String(e.textContent || '').includes(begin) && String(e.textContent || '').includes(end));
  if (users.length === 0) return {pending: true, reason: 'waiting-for-user-message'};
  if (users.length !== 1) return result('ambiguous-user-message');
  const user = users[0];
  const user_message_id = id(user), user_turn_id = turn(user);
  if (!user_message_id || !user_turn_id) return result('missing-user-binding');
  if (messages.filter(e => id(e) === user_message_id).length !== 1) return result('duplicate-message-id');
  const after = messages.slice(messages.indexOf(user) + 1);
  const nextUser = after.findIndex(e => role(e) === 'user');
  const assistants = nextUser < 0 ? after : after.slice(0, nextUser);
  if (assistants.length === 0) return {pending: true, reason: 'waiting-for-assistant'};
  if (assistants.some(e => role(e) !== 'assistant')) return result('non-contiguous-assistant-group');
  const assistant_turn_id = turn(assistants[0]);
  if (!assistant_turn_id || assistants.some(e => !id(e) || turn(e) !== assistant_turn_id))
    return result('assistant-turn-mismatch');
  if (new Set(assistants.map(id)).size !== assistants.length) return result('duplicate-message-id');
  const assistantIds = new Set(assistants.map(id));
  if (messages.some(e => assistantIds.has(id(e)) && !assistants.includes(e))) return result('duplicate-message-id');
  const busy = e => e.getAttribute('aria-busy') === 'true' ||
    [...e.querySelectorAll('[aria-busy="true"]')].length > 0;
  if (assistants.some(busy)) return {pending: true, reason: 'assistant-busy'};
  if (assistants.some(e => [...e.querySelectorAll('button[aria-label]')]
    .some(b => /stop/i.test(b.getAttribute('aria-label') || ''))))
    return {pending: true, reason: 'assistant-stop-control'};
  const stream = assistants.flatMap(e => [e.getAttribute('data-hatch-markdown-streaming'),
    ...[...e.querySelectorAll('[data-hatch-markdown-streaming]')]
      .map(x => x.getAttribute('data-hatch-markdown-streaming'))]).filter(x => x !== null);
  if (stream.includes('true') || !stream.includes('false'))
    return {pending: true, reason: 'assistant-streaming'};
  const pres = assistants.flatMap(e => [...e.querySelectorAll('pre')].map(pre => ({e, pre})));
  const final = assistants[assistants.length - 1];
  const assistant_message_id = id(final);
  if (!assistant_message_id) return result('missing-assistant-binding');
  const turn_messages = [user, ...assistants].map(e => ({
    message_id: id(e), turn_id: turn(e), role: role(e),
    terminal_status: e === final ? 'completed' : null,
    pre_nodes: [...e.querySelectorAll('pre')].map(pre => ({
      tag: 'pre', code_children: [...pre.querySelectorAll('code')].map(code => ({
        tag: 'code', class_name: typeof code.className === 'string' ? code.className : '',
        language: code.getAttribute('data-language') || '', text_content: code.textContent
      }))
    }))
  }));
  if (pres.length !== 1) return result('code-node-not-unique', {
    owner_id, thread_id, user_message_id, assistant_message_id, user_turn_id,
    assistant_turn_id, turn_messages, completed: true
  });
  if (busy(pres[0].pre)) return result('code-node-busy');
  const codes = [...pres[0].pre.querySelectorAll('code')];
  if (codes.length !== 1) return result('code-child-not-unique');
  const raw = codes[0].textContent;
  return {
    pending: false, owner_id, thread_id, user_message_id, assistant_message_id,
    user_turn_id, assistant_turn_id, turn_messages, completed: true,
    raw_ui_code_textcontent: raw, text_content: raw
  };
})"""


def capture_turn_expression(owner_id: str, operation_id: str, stage: str) -> str:
    payload = json.dumps({"owner_id": owner_id, "op_id": operation_id, "stage": stage}, ensure_ascii=False)
    return f"({_CAPTURE})({payload})"


_STOP = r"""(function(input) {
  const {op_id, stage} = input;
  const begin = `VFCTX BEGIN ${op_id} ${stage}`, end = `VFCTX END ${op_id} ${stage}`;
  const users = [...document.querySelectorAll('[data-message-id][data-message-role]')]
    .filter(e => e.getAttribute('data-message-role') === 'user' &&
      String(e.textContent || '').includes(begin) && String(e.textContent || '').includes(end));
  if (users.length !== 1) return false;
  const messages = [...document.querySelectorAll('[data-message-id][data-message-role]')];
  const after = messages.slice(messages.indexOf(users[0]) + 1);
  const nextUser = after.findIndex(e => e.getAttribute('data-message-role') === 'user');
  const group = nextUser < 0 ? after : after.slice(0, nextUser);
  if (!group.length || group.some(e => e.getAttribute('data-message-role') !== 'assistant')) return false;
  const turn = group[0].getAttribute('data-message-turn-id');
  if (!turn || group.some(e => e.getAttribute('data-message-turn-id') !== turn)) return false;
  // A dedicated browser tab is held by this operation. Only after verifying its
  // unique op marker and assistant turn may we click the page-level stop control.
  const buttons = [...document.querySelectorAll('button[aria-label*="Stop" i]')]
    .filter(b => b.getClientRects().length > 0);
  if (buttons.length !== 1) return false;
  const button = buttons[0];
  button.click();
  return true;
})"""


def stop_expression(operation_id: str, stage: str) -> str:
    return f"({_STOP})({json.dumps({'op_id': operation_id, 'stage': stage})})"
