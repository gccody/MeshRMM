import assert from 'node:assert/strict';
import test from 'node:test';
import { actionErrorsReducer, listActionErrors } from '../features/workspace/action-errors.ts';

test('each source owns its error; clearing one leaves the others', () => {
  let state = {};
  state = actionErrorsReducer(state, { source: 'delete', message: 'The Agent could not be deleted.' });
  state = actionErrorsReducer(state, { source: 'remote', message: 'The remote session could not be started.' });
  state = actionErrorsReducer(state, { source: 'resume', message: 'Your session could not be resumed.' });
  // A new remote attempt clears only its own error.
  state = actionErrorsReducer(state, { source: 'remote', message: null });
  assert.deepEqual(state, {
    delete: 'The Agent could not be deleted.',
    resume: 'Your session could not be resumed.',
  });
});

test('no-op updates keep the same state object', () => {
  const state = actionErrorsReducer({}, { source: 'delete', message: 'Denied' });
  assert.equal(actionErrorsReducer(state, { source: 'delete', message: 'Denied' }), state);
  assert.equal(actionErrorsReducer(state, { source: 'remote', message: null }), state);
  assert.notEqual(actionErrorsReducer(state, { source: 'delete', message: 'Denied again' }), state);
});

test('lists errors in a stable order, filtered by source', () => {
  const state = { resume: 'R', delete: 'D', remote: 'C' };
  assert.deepEqual(listActionErrors(state), [
    { source: 'remote', message: 'C' },
    { source: 'delete', message: 'D' },
    { source: 'resume', message: 'R' },
  ]);
  assert.deepEqual(listActionErrors(state, ['delete', 'close-session']), [{ source: 'delete', message: 'D' }]);
  assert.deepEqual(listActionErrors({}), []);
});
