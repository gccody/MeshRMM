import AuthenticationServices
import CryptoKit
import Foundation
import UIKit

@MainActor
final class AuthService: NSObject, ASWebAuthenticationPresentationContextProviding {
    static let redirectURI = "meshrmm-ios://auth/callback"

    private let keychain = KeychainStore()
    private var webSession: ASWebAuthenticationSession?
    private let account = "workos-tokens"

    func storedAuthentication() -> WorkOSAuthentication? {
        keychain.load(account: account).flatMap { try? JSONDecoder().decode(WorkOSAuthentication.self, from: $0) }
    }

    func signIn(config: MobileClientConfig) async throws -> WorkOSAuthentication {
        let verifier = Self.randomURLSafeString(byteCount: 32)
        let state = Self.randomURLSafeString(byteCount: 24)
        let challenge = Data(SHA256.hash(data: Data(verifier.utf8))).base64URLEncodedString()
        var components = URLComponents(string: "https://api.workos.com/user_management/authorize")!
        components.queryItems = [
            URLQueryItem(name: "client_id", value: config.workOSClientID),
            URLQueryItem(name: "redirect_uri", value: Self.redirectURI),
            URLQueryItem(name: "response_type", value: "code"),
            URLQueryItem(name: "provider", value: "authkit"),
            URLQueryItem(name: "organization_id", value: config.workOSOrganizationID),
            URLQueryItem(name: "state", value: state),
            URLQueryItem(name: "code_challenge", value: challenge),
            URLQueryItem(name: "code_challenge_method", value: "S256"),
        ]
        guard let authorizationURL = components.url else { throw MeshError.invalidResponse }
        let callback = try await authenticate(url: authorizationURL)
        let callbackComponents = URLComponents(url: callback, resolvingAgainstBaseURL: false)
        let values = Dictionary(uniqueKeysWithValues: (callbackComponents?.queryItems ?? []).map { ($0.name, $0.value ?? "") })
        if let message = values["error_description"], !message.isEmpty { throw MeshError.server(message) }
        guard values["state"] == state, let code = values["code"], !code.isEmpty else {
            throw MeshError.authenticationRequired
        }
        let authentication = try await exchange([
            "client_id": config.workOSClientID,
            "grant_type": "authorization_code",
            "code": code,
            "code_verifier": verifier,
        ])
        try save(authentication)
        return authentication
    }

    func validAccessToken(config: MobileClientConfig) async throws -> String {
        guard let authentication = storedAuthentication() else { throw MeshError.authenticationRequired }
        if Self.jwtExpiration(authentication.accessToken).map({ $0 > Date().addingTimeInterval(90) }) == true {
            return authentication.accessToken
        }
        do {
            let refreshed = try await exchange([
                "client_id": config.workOSClientID,
                "grant_type": "refresh_token",
                "refresh_token": authentication.refreshToken,
                "organization_id": config.workOSOrganizationID,
            ])
            try save(refreshed)
            return refreshed.accessToken
        } catch {
            signOut()
            throw MeshError.authenticationRequired
        }
    }

    func signOut() {
        keychain.delete(account: account)
    }

    func presentationAnchor(for session: ASWebAuthenticationSession) -> ASPresentationAnchor {
        UIApplication.shared.connectedScenes
            .compactMap { $0 as? UIWindowScene }
            .flatMap(\.windows)
            .first(where: \.isKeyWindow) ?? ASPresentationAnchor()
    }

    private func authenticate(url: URL) async throws -> URL {
        try await withCheckedThrowingContinuation { continuation in
            let session = ASWebAuthenticationSession(url: url, callbackURLScheme: "meshrmm-ios") { [weak self] callback, error in
                self?.webSession = nil
                if let error { continuation.resume(throwing: error) }
                else if let callback { continuation.resume(returning: callback) }
                else { continuation.resume(throwing: MeshError.authenticationRequired) }
            }
            session.presentationContextProvider = self
            session.prefersEphemeralWebBrowserSession = false
            webSession = session
            guard session.start() else {
                webSession = nil
                continuation.resume(throwing: MeshError.authenticationRequired)
                return
            }
        }
    }

    private func exchange(_ body: [String: String]) async throws -> WorkOSAuthentication {
        var request = URLRequest(url: URL(string: "https://api.workos.com/user_management/authenticate")!)
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONEncoder().encode(body)
        let (data, response) = try await URLSession.shared.data(for: request)
        guard let http = response as? HTTPURLResponse else { throw MeshError.invalidResponse }
        guard (200..<300).contains(http.statusCode) else {
            let message = (try? JSONDecoder().decode(APIErrorBody.self, from: data).error) ?? "WorkOS sign-in failed (HTTP \(http.statusCode))."
            throw MeshError.server(message)
        }
        return try JSONDecoder().decode(WorkOSAuthentication.self, from: data)
    }

    private func save(_ authentication: WorkOSAuthentication) throws {
        try keychain.save(JSONEncoder().encode(authentication), account: account)
    }

    private static func randomURLSafeString(byteCount: Int) -> String {
        var bytes = [UInt8](repeating: 0, count: byteCount)
        _ = SecRandomCopyBytes(kSecRandomDefault, bytes.count, &bytes)
        return Data(bytes).base64URLEncodedString()
    }

    static func jwtExpiration(_ token: String) -> Date? {
        let parts = token.split(separator: ".")
        guard parts.count > 1,
              let data = Data(base64URLEncoded: String(parts[1])),
              let object = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let seconds = object["exp"] as? TimeInterval else { return nil }
        return Date(timeIntervalSince1970: seconds)
    }
}

extension Data {
    init?(base64URLEncoded value: String) {
        var base64 = value.replacingOccurrences(of: "-", with: "+").replacingOccurrences(of: "_", with: "/")
        base64.append(String(repeating: "=", count: (4 - base64.count % 4) % 4))
        self.init(base64Encoded: base64)
    }

    func base64URLEncodedString() -> String {
        base64EncodedString().replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_").replacingOccurrences(of: "=", with: "")
    }
}
