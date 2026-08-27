import Foundation

@MainActor
final class AppModel: ObservableObject {
    @Published var workspace = UserDefaults.standard.string(forKey: "workspace") ?? ""
    @Published private(set) var config: MobileClientConfig?
    @Published private(set) var devices: [ManagedDevice] = []
    @Published var selectedDevice: ManagedDevice?
    @Published private(set) var isSignedIn = false
    @Published private(set) var isBusy = false
    @Published var errorMessage: String?

    let auth = AuthService()
    lazy var api = APIClient(auth: auth)
    private var eventsTask: Task<Void, Never>?
    private var revision = -1

    var onlineCount: Int { devices.lazy.filter(\.connected).count }

    func restore() async {
        guard !workspace.isEmpty else { return }
        do {
            let workspaceURL = try URL.meshWorkspace(from: workspace)
            let loaded = try await api.mobileConfig(workspace: workspaceURL)
            config = loaded
            workspace = loaded.apiURL.absoluteString
            isSignedIn = auth.storedAuthentication() != nil
            if isSignedIn { try await refresh() }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func connectWorkspace() async {
        isBusy = true
        errorMessage = nil
        defer { isBusy = false }
        do {
            let workspaceURL = try URL.meshWorkspace(from: workspace)
            let loaded = try await api.mobileConfig(workspace: workspaceURL)
            config = loaded
            workspace = loaded.apiURL.absoluteString
            UserDefaults.standard.set(workspace, forKey: "workspace")
            isSignedIn = auth.storedAuthentication() != nil
            if isSignedIn { try await refresh() }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func signIn() async {
        guard let config else { return }
        isBusy = true
        errorMessage = nil
        defer { isBusy = false }
        do {
            let authentication = try await auth.signIn(config: config)
            guard authentication.organizationID == nil || authentication.organizationID == config.workOSOrganizationID else {
                auth.signOut()
                throw MeshError.server("The signed-in organization does not match this company workspace.")
            }
            isSignedIn = true
            try await refresh()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    func refresh() async throws {
        guard let config else { return }
        isBusy = devices.isEmpty
        defer { isBusy = false }
        do {
            let response = try await api.listDevices(config: config)
            revision = response.revision
            devices = Self.sorted(response.agents)
            startEvents()
        } catch MeshError.authenticationRequired {
            signOut()
            throw MeshError.authenticationRequired
        }
    }

    func signOut() {
        eventsTask?.cancel()
        eventsTask = nil
        auth.signOut()
        isSignedIn = false
        devices = []
        revision = -1
    }

    func forgetWorkspace() {
        signOut()
        config = nil
        workspace = ""
        UserDefaults.standard.removeObject(forKey: "workspace")
    }

    private func startEvents() {
        guard eventsTask == nil, let config else { return }
        eventsTask = Task { [weak self] in
            var delay: UInt64 = 1_000_000_000
            while !Task.isCancelled {
                do {
                    guard let self else { return }
                    let subscription = try await self.api.eventSubscription(config: config)
                    var components = URLComponents(url: subscription.websocketURL, resolvingAgainstBaseURL: false)!
                    components.queryItems = (components.queryItems ?? []) + [URLQueryItem(name: "token", value: subscription.subscriptionToken)]
                    guard let url = components.url else { throw MeshError.invalidResponse }
                    guard let origin = config.apiURL.meshOrigin else { throw MeshError.invalidResponse }
                    var socketRequest = URLRequest(url: url)
                    socketRequest.setValue(origin, forHTTPHeaderField: "Origin")
                    let socket = URLSession.shared.webSocketTask(with: socketRequest)
                    socket.resume()
                    delay = 1_000_000_000
                    while !Task.isCancelled {
                        let message = try await socket.receive()
                        guard case .string(let text) = message, let data = text.data(using: .utf8) else { continue }
                        let event = try JSONDecoder().decode(AgentEvent.self, from: data)
                        self.apply(event)
                    }
                    socket.cancel(with: .normalClosure, reason: nil)
                } catch {
                    if Task.isCancelled { return }
                    try? await Task.sleep(nanoseconds: delay)
                    delay = min(delay * 2, 30_000_000_000)
                }
            }
        }
    }

    private func apply(_ event: AgentEvent) {
        if event.type == "snapshot", let agents = event.agents, event.revision >= revision {
            revision = event.revision
            devices = Self.sorted(agents)
        } else if event.type == "agent_upsert", let agent = event.agent, event.revision == revision + 1 {
            revision = event.revision
            devices = Self.sorted(devices.filter { $0.id != agent.id } + [agent])
        } else if event.type == "agent_deleted", let id = event.agentID, event.revision == revision + 1 {
            revision = event.revision
            devices.removeAll { $0.id == id }
        }
    }

    private static func sorted(_ devices: [ManagedDevice]) -> [ManagedDevice] {
        devices.sorted {
            if $0.connected != $1.connected { return $0.connected }
            let nameOrder = $0.name.localizedCaseInsensitiveCompare($1.name)
            return nameOrder == .orderedSame ? $0.id < $1.id : nameOrder == .orderedAscending
        }
    }
}
