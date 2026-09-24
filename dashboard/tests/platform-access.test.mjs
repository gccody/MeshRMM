import assert from 'node:assert/strict';
import test from 'node:test';
import {
  errorAfterFailure,
  isAccessDenied,
  ownerAccessAfterFailure,
  PlatformRequestError,
} from '../features/platform/platform-access.ts';

test('only 401 and 403 refuse platform owner access', () => {
  for (const status of [401, 403]) assert.equal(isAccessDenied(new PlatformRequestError('x', status)), true);
  for (const status of [400, 404, 408, 429, 500, 503]) assert.equal(isAccessDenied(new PlatformRequestError('x', status)), false);
  assert.equal(isAccessDenied(new TypeError('Failed to fetch')), false);
  assert.equal(isAccessDenied(new Error('Your administrator session has expired.')), false);
});

test('an outage keeps the known access state instead of reporting missing access', () => {
  const outage = new PlatformRequestError('sign-in could not be checked right now', 503);
  assert.equal(ownerAccessAfterFailure(null, outage), null);
  assert.equal(ownerAccessAfterFailure(true, outage), true);
  assert.equal(ownerAccessAfterFailure(true, new TypeError('Failed to fetch')), true);
  assert.equal(ownerAccessAfterFailure(null, new PlatformRequestError('platform owner access is required', 403)), false);
  assert.equal(ownerAccessAfterFailure(true, new PlatformRequestError('Your administrator session has expired.', 401)), false);
});

test('a reload after a failed create keeps the create error visible', () => {
  const duplicate = 'that company slug is already reserved';
  assert.equal(errorAfterFailure(duplicate, new PlatformRequestError('Service unavailable', 503), 'Service unavailable'), duplicate);
  assert.equal(errorAfterFailure(null, new PlatformRequestError('Service unavailable', 503), 'Service unavailable'), 'Service unavailable');
  const denied = new PlatformRequestError('platform owner access is required', 403);
  assert.equal(errorAfterFailure(duplicate, denied, denied.message), denied.message);
});
