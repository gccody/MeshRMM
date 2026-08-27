import Foundation

final class APIClient {
    private let auth: AuthService

    init(auth: AuthService) {
        self.auth = auth
    }

    func mobileConfig(workspace: URL) async throws -> MobileClientConfig {
        let request = URLRequest(url: workspace.appendingAPIPath("v1/mobile/config"), cachePolicy: .reloadIgnoringLocalCacheData)
        return try await send(request, authenticatedWith: nil, as: MobileClientConfig.self)
    }

    @MainActor
    func listDevices(config: MobileClientConfig) async throws -> AgentListResponse {
        var request = URLRequest(url: config.apiURL.appendingAPIPath("v1/agents"), cachePolicy: .reloadIgnoringLocalCacheData)
        request.setValue("Bearer \(try await auth.validAccessToken(config: config))", forHTTPHeaderField: "Authorization")
        return try await send(request, authenticatedWith: config, as: AgentListResponse.self)
    }

    @MainActor
    func createSession(deviceID: String, config: MobileClientConfig) async throws -> (URL, SessionBootstrap) {
        var handoffRequest = URLRequest(url: config.apiURL.appendingAPIPath("v1/remote/handoffs"))
        handoffRequest.httpMethod = "POST"
        handoffRequest.setValue("application/json", forHTTPHeaderField: "Content-Type")
        handoffRequest.setValue("Bearer \(try await auth.validAccessToken(config: config))", forHTTPHeaderField: "Authorization")
        handoffRequest.httpBody = try JSONSerialization.data(withJSONObject: ["device_id": deviceID])
        let handoff = try await send(handoffRequest, authenticatedWith: config, as: HandoffResponse.self)

        var redeemRequest = URLRequest(url: handoff.apiURL.appendingAPIPath("v1/remote/handoffs/redeem"))
        redeemRequest.httpMethod = "POST"
        redeemRequest.setValue("Bearer \(handoff.handoffToken)", forHTTPHeaderField: "Authorization")
        let bootstrap = try await send(redeemRequest, authenticatedWith: nil, as: SessionBootstrap.self)
        return (handoff.apiURL, bootstrap)
    }

    @MainActor
    func resumeSession(apiURL: URL, bootstrap: SessionBootstrap) async throws -> (URL, SessionBootstrap) {
        var request = URLRequest(url: apiURL.appendingAPIPath(
            "v1/remote/sessions/\(bootstrap.sessionID)/resume"
        ))
        request.httpMethod = "POST"
        request.setValue("Bearer \(bootstrap.signalingToken)", forHTTPHeaderField: "Authorization")
        let refreshed = try await send(request, authenticatedWith: nil, as: SessionBootstrap.self)
        return (apiURL, refreshed)
    }

    @MainActor
    func eventSubscription(config: MobileClientConfig) async throws -> AgentEventSubscription {
        var request = URLRequest(url: config.apiURL.appendingAPIPath("v1/agents/events/subscriptions"))
        request.httpMethod = "POST"
        request.setValue("Bearer \(try await auth.validAccessToken(config: config))", forHTTPHeaderField: "Authorization")
        return try await send(request, authenticatedWith: config, as: AgentEventSubscription.self)
    }

    private func send<T: Decodable>(_ request: URLRequest, authenticatedWith config: MobileClientConfig?, as: T.Type) async throws -> T {
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw MeshError.invalidResponse }
        guard (200..<300).contains(http.statusCode) else {
            if http.statusCode == 401, config != nil { throw MeshError.authenticationRequired }
            let message = (try? JSONDecoder().decode(APIErrorBody.self, from: data).error) ?? "MeshRMM returned HTTP \(http.statusCode)."
            throw MeshError.server(message)
        }
        do { return try JSONDecoder().decode(T.self, from: data) }
        catch { throw MeshError.invalidResponse }
    }
}

struct AgentEventSubscription: Codable {
    let subscriptionToken: String
    let websocketURL: URL
    let expiresAtUnixMilliseconds: UInt64

    enum CodingKeys: String, CodingKey {
        case subscriptionToken = "subscription_token"
        case websocketURL = "websocket_url"
        case expiresAtUnixMilliseconds = "expires_at_unix_ms"
    }
}

struct AgentEvent: Codable {
    let type: String
    let revision: Int
    let agents: [ManagedDevice]?
    let agent: ManagedDevice?
    let agentID: String?

    enum CodingKeys: String, CodingKey {
        case type, revision, agents, agent
        case agentID = "agent_id"
    }
}
