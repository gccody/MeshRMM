import assert from 'node:assert/strict';
import test from 'node:test';
import { actionErrorsReducer, listActionErrors } from '../features/workspace/action-errors.ts';

test('each source owns its error; clearing one leaves the others', () => {
  let state = {};
  state = actionErrorsReducer(state, { source: 'delete', message: 'The Agent could not be deleted.' });
  state = actionErrorsReducer(state, { source: 'remote', message: 'The remote session could not be started.' });
  state = actionErrorsReducer(state, { source: 'close-session', message: 'The remote session could not be closed.' });
  // A new remote attempt clears only its own error.
  state = actionErrorsReducer(state, { source: 'remote', message: null });
  assert.deepEqual(state, {
    delete: 'The Agent could not be deleted.',
    'close-session': 'The remote session could not be closed.',
  });
});

test('no-op updates keep the same state object', () => {
  const state = actionErrorsReducer({}, { source: 'delete', message: 'Denied' });
  assert.equal(actionErrorsReducer(state, { source: 'delete', message: 'Denied' }), state);
  assert.equal(actionErrorsReducer(state, { source: 'remote', message: null }), state);
  assert.notEqual(actionErrorsReducer(state, { source: 'delete', message: 'Denied again' }), state);
});

test('lists errors in a stable order, filtered by source', () => {
  const state = { 'close-session': 'S', delete: 'D', remote: 'C' };
  assert.deepEqual(listActionErrors(state), [
    { source: 'remote', message: 'C' },
    { source: 'close-session', message: 'S' },
    { source: 'delete', message: 'D' },
  ]);
  assert.deepEqual(listActionErrors(state, ['delete']), [{ source: 'delete', message: 'D' }]);
  assert.deepEqual(listActionErrors({}), []);
});
