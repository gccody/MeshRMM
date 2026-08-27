import Foundation

struct MobileClientConfig: Codable, Equatable {
    let apiURL: URL
    let companyName: String
    let workOSClientID: String
    let workOSOrganizationID: String

    enum CodingKeys: String, CodingKey {
        case apiURL = "api_url"
        case companyName = "company_name"
        case workOSClientID = "workos_client_id"
        case workOSOrganizationID = "workos_organization_id"
    }
}

struct ManagedDevice: Codable, Identifiable, Equatable {
    let id: String
    let name: String
    let connected: Bool
}

struct AgentListResponse: Codable {
    let agents: [ManagedDevice]
    let revision: Int
}

struct HandoffResponse: Codable {
    let handoffToken: String
    let apiURL: URL
    let expiresAtUnixMilliseconds: UInt64

    enum CodingKeys: String, CodingKey {
        case handoffToken = "handoff_token"
        case apiURL = "api_url"
        case expiresAtUnixMilliseconds = "expires_at_unix_ms"
    }
}

struct IceServer: Codable, Equatable {
    let urls: [String]
    let username: String?
    let credential: String?
}

struct SessionBootstrap: Codable, Equatable {
    let sessionID: String
    let signalingToken: String
    let expiresAtUnixMilliseconds: UInt64
    let iceServers: [IceServer]

    enum CodingKeys: String, CodingKey {
        case sessionID = "session_id"
        case signalingToken = "signaling_token"
        case expiresAtUnixMilliseconds = "expires_at_unix_ms"
        case iceServers = "ice_servers"
    }
}

struct WorkOSAuthentication: Codable {
    let accessToken: String
    let refreshToken: String
    let organizationID: String?

    enum CodingKeys: String, CodingKey {
        case accessToken = "access_token"
        case refreshToken = "refresh_token"
        case organizationID = "organization_id"
    }
}

struct APIErrorBody: Codable {
    let error: String
}

enum MeshError: LocalizedError {
    case invalidWorkspace
    case invalidResponse
    case authenticationRequired
    case server(String)
    case remote(String)

    var errorDescription: String? {
        switch self {
        case .invalidWorkspace: "Enter your company dashboard, such as acme.meshrmm.com."
        case .invalidResponse: "The service returned an invalid response."
        case .authenticationRequired: "Your MeshRMM session has expired. Sign in again."
        case .server(let message), .remote(let message): message
        }
    }
}

extension URL {
    static func meshWorkspace(from input: String) throws -> URL {
        var value = input.trimmingCharacters(in: .whitespacesAndNewlines)
        if !value.contains("://") { value = "https://\(value)" }
        guard var components = URLComponents(string: value),
              components.scheme?.lowercased() == "https",
              let host = components.host?.lowercased(), !host.isEmpty else {
            throw MeshError.invalidWorkspace
        }
        components.scheme = "https"
        components.host = host
        components.path = ""
        components.query = nil
        components.fragment = nil
        guard let url = components.url else { throw MeshError.invalidWorkspace }
        return url
    }

    func appendingAPIPath(_ path: String) -> URL {
        appending(path: path.trimmingCharacters(in: CharacterSet(charactersIn: "/")))
    }

    var meshOrigin: String? {
        guard var components = URLComponents(url: self, resolvingAgainstBaseURL: false),
              components.scheme != nil, components.host != nil else { return nil }
        components.user = nil
        components.password = nil
        components.path = ""
        components.query = nil
        components.fragment = nil
        return components.url?.absoluteString
    }
}
