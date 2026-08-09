import 'dart:convert';

import 'package:flutter_test/flutter_test.dart';
import 'package:flutter_hbb/models/screencam_policy.dart';

void main() {
  group('ScreenCam display policy validation', () {
    test('accepts the strict Windows display identifier', () {
      expect(isValidScreenCamDisplayId(r'\\.\DISPLAY1'), isTrue);
      expect(isValidScreenCamDisplayId(r'\\.\display0002'), isTrue);
      expect(isValidScreenCamDisplayId(r'\\.\DISPLAY4294967295'), isTrue);
    });

    test('rejects malformed, empty and overflowing display identifiers', () {
      for (final value in [
        '',
        ' ',
        r' \\.\DISPLAY1',
        '${r'\\.\DISPLAY1'}\n',
        r'\\.\MONITOR1',
        r'\\.\DISPLAY',
        r'\\.\DISPLAY0',
        r'\\.\DISPLAY4294967296',
        r'\\.\DISPLAY00000000001',
        r'\\.\DISPLAY1-extra',
        '\\\\.\\D\u0131SPLAY1',
        '\\\\.\\D\u017FPLAY1',
      ]) {
        expect(isValidScreenCamDisplayId(value), isFalse, reason: value);
      }
    });

    // Kept in lockstep with the Rust mirror in
    // `src/server/screen_cam/display.rs`
    // (`rejects_control_characters_signs_and_non_ascii_digits`) so both sides
    // of the policy IPC accept and reject exactly the same values.
    test('rejects control characters, signs and non-ASCII digits', () {
      for (final value in [
        '\\\\.\\DISPLAY1\u0000',
        '\\\\.\\DISPLAY\u00001',
        '\\\\.\\DISPLAY1\t',
        '\\\\.\\DISPLAY\t1',
        r'\\.\DISPLAY+1',
        r'\\.\DISPLAY-1',
        r'\\.\DISPLAY 1',
        r'\\.\DISPLAY1 2',
        '\\\\.\\DISPLAY\u0661',
        '\\\\.\\DISPLAY\uFF11',
      ]) {
        expect(isValidScreenCamDisplayId(value), isFalse, reason: value);
      }
    });

    test('keeps independently valid partial policy fields', () {
      expect(
        screenCamDisplayPolicyValues({
          'selected_display_id': r'\\.\DISPLAY2',
          'fallback_to_primary': true,
        }),
        {
          'screencam-selected-display-id': r'\\.\DISPLAY2',
          'screencam-fallback-to-primary': 'Y',
        },
      );
      expect(
        screenCamDisplayPolicyValues({
          'selected_display_id': 'invalid',
          'fallback_to_primary': false,
        }),
        {'screencam-fallback-to-primary': 'N'},
      );
      expect(screenCamDisplayPolicyValues({}), isEmpty);
    });

    test('partial updates preserve historical ScreenCam configuration',
        () async {
      final stored = <String, Object?>{
        'licensed': true,
        'desired_state': 'running',
        'mode': 'managed',
        'rtsp_user': 'seh_123abc',
        'rtsp_password': 'AbCd1234EfGh',
        'selected_display_id': r'\\.\DISPLAY1',
        'fallback_to_primary': true,
      };
      var sends = 0;
      Future<String> sender(String payload) async {
        sends++;
        final update = jsonDecode(payload) as Map<String, dynamic>;
        var changed = false;
        for (final key in ['selected_display_id', 'fallback_to_primary']) {
          if (update.containsKey(key) && stored[key] != update[key]) {
            stored[key] = update[key];
            changed = true;
          }
        }
        return jsonEncode({
          'applied': true,
          'changed': changed,
          'error': null,
        });
      }

      expect(
        (await applyScreenCamDisplayPolicyUpdate(
          {'selected_display_id': r'\\.\DISPLAY2'},
          sender,
        ))
            .changed,
        isTrue,
      );
      expect(stored['selected_display_id'], r'\\.\DISPLAY2');
      expect(stored['licensed'], isTrue);
      expect(stored['desired_state'], 'running');
      expect(stored['mode'], 'managed');
      expect(stored['rtsp_user'], 'seh_123abc');
      expect(stored['rtsp_password'], 'AbCd1234EfGh');

      expect(
        (await applyScreenCamDisplayPolicyUpdate(
          {'fallback_to_primary': false},
          sender,
        ))
            .changed,
        isTrue,
      );
      expect(stored['fallback_to_primary'], isFalse);

      expect(
        (await applyScreenCamDisplayPolicyUpdate(
          {
            'selected_display_id': r'\\.\DISPLAY3',
            'fallback_to_primary': true,
          },
          sender,
        ))
            .changed,
        isTrue,
      );
      expect(stored['selected_display_id'], r'\\.\DISPLAY3');
      expect(stored['fallback_to_primary'], isTrue);

      await applyScreenCamDisplayPolicyUpdate(
        {
          'selected_display_id': 'invalid',
          'fallback_to_primary': false,
        },
        sender,
      );
      expect(stored['selected_display_id'], r'\\.\DISPLAY3');
      expect(stored['fallback_to_primary'], isFalse);

      final sendsBeforeEmpty = sends;
      final empty = await applyScreenCamDisplayPolicyUpdate({}, sender);
      expect(empty.attempted, isFalse);
      expect(sends, sendsBeforeEmpty);

      final duplicate = await applyScreenCamDisplayPolicyUpdate(
        {'fallback_to_primary': false},
        sender,
      );
      expect(duplicate.applied, isTrue);
      expect(duplicate.changed, isFalse);
    });

    test('NACK and malformed ACK are never reported as applied', () async {
      final nack = await applyScreenCamDisplayPolicyUpdate(
        {'fallback_to_primary': true},
        (_) async => jsonEncode({
          'applied': false,
          'changed': false,
          'error': 'persistence_failed',
        }),
      );
      expect(nack.applied, isFalse);
      expect(nack.error, 'persistence_failed');

      final malformed = await applyScreenCamDisplayPolicyUpdate(
        {'fallback_to_primary': true},
        (_) async => '{}',
      );
      expect(malformed.applied, isFalse);
      expect(malformed.error, 'ipc_unavailable');

      for (final response in [
        '',
        'not-json',
        '{"applied":false,"changed":false,"error":"internal_detail"}',
        '{"applied":false,"changed":false,"error":null}',
        '{"applied":true,"changed":true,"error":"persistence_failed"}',
      ]) {
        final result = await applyScreenCamDisplayPolicyUpdate(
          {'fallback_to_primary': true},
          (_) async => response,
        );
        expect(result.applied, isFalse, reason: response);
        expect(result.changed, isFalse, reason: response);
        expect(result.error, 'ipc_unavailable', reason: response);
      }

      final exception = await applyScreenCamDisplayPolicyUpdate(
        {'fallback_to_primary': true},
        (_) async => throw StateError('closed'),
      );
      expect(exception.error, 'ipc_unavailable');
    });

    test('partial WebSocket payloads never default historical fields', () {
      // A complete block is applied in full.
      expect(
        screenCamHistoricalPolicyValues({
          'licensed': true,
          'desired_state': 'running',
          'mode': 'supervised',
          'rtsp_user': 'seh_123abc',
          'rtsp_password': 'AbCd1234EfGh',
        }),
        {
          'screencam-licensed': 'Y',
          'screencam-desired-state': 'running',
          'screencam-mode': 'supervised',
          'screencam-rtsp-user': 'seh_123abc',
          'screencam-rtsp-pass': 'AbCd1234EfGh',
        },
      );

      // A display-only push must not touch a single historical key: absent
      // fields would otherwise become N / stopped / local and wipe credentials.
      expect(
        screenCamHistoricalPolicyValues({
          'selected_display_id': r'\\.\DISPLAY2',
          'fallback_to_primary': true,
        }),
        isEmpty,
      );
      expect(screenCamHistoricalPolicyValues({}), isEmpty);

      // Present-but-false is a real value, not an absence.
      expect(
        screenCamHistoricalPolicyValues({'licensed': false}),
        {'screencam-licensed': 'N'},
      );

      // An explicit null still revokes a credential; an absent key does not.
      expect(
        screenCamHistoricalPolicyValues({'rtsp_user': null}),
        {'screencam-rtsp-user': ''},
      );
      expect(
        screenCamHistoricalPolicyValues({'rtsp_password': null}),
        {'screencam-rtsp-pass': ''},
      );

      // Out-of-contract values are dropped rather than forwarded.
      expect(
        screenCamHistoricalPolicyValues({
          'licensed': 'yes',
          'desired_state': 'paused',
          'mode': 'unknown',
        }),
        isEmpty,
      );
    });

    test('port overrides only reach the daemon when they are usable', () {
      expect(
        screenCamHistoricalPolicyValues({'rtsp_port_override': 8554, 'onvif_port_override': 8080}),
        {
          'screencam-policy-rtsp-port': '8554',
          'screencam-policy-onvif-port': '8080',
        },
      );

      // Absent means "no opinion" — the device keeps 554/80, or whatever its
      // own config says. Only an absent key leaves a stored override alone.
      expect(screenCamHistoricalPolicyValues({'licensed': true}),
          {'screencam-licensed': 'Y'});

      // An explicit null clears a previously issued override, same as for the
      // credentials.
      expect(
        screenCamHistoricalPolicyValues({'rtsp_port_override': null}),
        {'screencam-policy-rtsp-port': ''},
      );

      // A panel that sends a port the daemon can't bind must not be able to
      // take the RTSP listener down: 0 is ephemeral and useless to an NVR,
      // and anything out of range or non-numeric is not a port at all.
      for (final invalid in [0, -1, 65536, 'abc', true, 1.5]) {
        expect(
          screenCamHistoricalPolicyValues({'onvif_port_override': invalid}),
          {'screencam-policy-onvif-port': ''},
          reason: 'rejects $invalid',
        );
      }

      // Numeric strings are accepted — JSON from the panel is not guaranteed
      // to type ports as integers.
      expect(
        screenCamHistoricalPolicyValues({'rtsp_port_override': '1554'}),
        {'screencam-policy-rtsp-port': '1554'},
      );
      expect(
        screenCamHistoricalPolicyValues({'rtsp_port_override': 65535}),
        {'screencam-policy-rtsp-port': '65535'},
      );
    });

    test('V2 resolution merges fields once with independent precedence',
        () async {
      expect(screenCamV2PolicyDecision(404, null).policy, isNull);
      expect(screenCamV2PolicyDecision(500, {}).policy, isNull);
      expect(screenCamV2PolicyDecision(200, 'malformed-shape').policy, isNull);
      expect(
        screenCamV2PolicyDecision(200, {
          'device_uid_resolved': false,
          'screen_cam': {'fallback_to_primary': false},
        }).unresolved,
        isTrue,
      );

      final resolved = screenCamV2PolicyDecision(200, {
        'device_uid_resolved': true,
        'screen_cam': {
          'selected_display_id': r'\\.\DISPLAY2',
          'fallback_to_primary': 'invalid',
        },
      });
      expect(resolved.unresolved, isFalse);
      expect(
        resolveScreenCamDisplayPolicy(
          {
            'selected_display_id': r'\\.\DISPLAY1',
            'fallback_to_primary': false,
          },
          resolved,
        ),
        {
          'selected_display_id': r'\\.\DISPLAY2',
          'fallback_to_primary': false,
        },
      );
      expect(
        resolveScreenCamDisplayPolicy(
          {
            'selected_display_id': r'\\.\DISPLAY1',
            'fallback_to_primary': false,
          },
          const ScreenCamV2PolicyDecision(policy: {
            'selected_display_id': 'invalid',
            'fallback_to_primary': true,
          }),
        ),
        {
          'selected_display_id': r'\\.\DISPLAY1',
          'fallback_to_primary': true,
        },
      );

      final stored = <String, Object?>{
        'selected_display_id': r'\\.\DISPLAY1',
        'fallback_to_primary': false,
      };
      Future<String> sender(String payload) async {
        final update = jsonDecode(payload) as Map<String, dynamic>;
        stored.addAll(update);
        return '{"applied":true,"changed":true,"error":null}';
      }

      var sends = 0;
      Future<String> countingSender(String payload) async {
        sends++;
        return sender(payload);
      }

      final finalPolicy = resolveScreenCamDisplayPolicy(
        {
          'selected_display_id': r'\\.\DISPLAY1',
          'fallback_to_primary': false,
        },
        resolved,
      );
      await applyScreenCamDisplayPolicyUpdate(finalPolicy, countingSender);
      expect(stored['selected_display_id'], r'\\.\DISPLAY2');
      expect(stored['fallback_to_primary'], isFalse);
      expect(sends, 1);

      for (final decision in [
        const ScreenCamV2PolicyDecision(),
        const ScreenCamV2PolicyDecision(unresolved: true),
        screenCamV2PolicyDecision(404, null),
        screenCamV2PolicyDecision(200, 'invalid-json-shape'),
      ]) {
        expect(
          resolveScreenCamDisplayPolicy(
            {
              'selected_display_id': r'\\.\DISPLAY7',
              'fallback_to_primary': true,
            },
            decision,
          ),
          {
            'selected_display_id': r'\\.\DISPLAY7',
            'fallback_to_primary': true,
          },
        );
      }
    });
  });
}
