import XCTest
@testable import MeshRMM

final class APIModelsTests: XCTestCase {
    func testWorkspaceNormalization() throws {
        XCTAssertEqual(try URL.meshWorkspace(from: " Acme.MeshRMM.com/dashboard ").absoluteString, "https://acme.meshrmm.com")
        XCTAssertThrowsError(try URL.meshWorkspace(from: "http://acme.meshrmm.com"))
    }

    func testOriginDropsCredentialsPathAndQuery() throws {
        let url = try XCTUnwrap(URL(string: "https://user:secret@internal.meshrmm.com:8443/v1/agents?token=secret"))
        XCTAssertEqual(url.meshOrigin, "https://internal.meshrmm.com:8443")
    }

    func testDecodesSessionBootstrap() throws {
        let data = Data(#"{"session_id":"s1","signaling_token":"secret","expires_at_unix_ms":99,"ice_servers":[{"urls":["stun:example.test"]}]}"#.utf8)
        let value = try JSONDecoder().decode(SessionBootstrap.self, from: data)
        XCTAssertEqual(value.sessionID, "s1")
        XCTAssertEqual(value.iceServers.first?.urls, ["stun:example.test"])
    }
}
