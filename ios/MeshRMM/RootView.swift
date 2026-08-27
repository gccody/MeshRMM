import SwiftUI

struct RootView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        Group {
            if model.config == nil {
                WorkspaceView()
            } else if !model.isSignedIn {
                SignInView()
            } else {
                DeviceListView()
            }
        }
        .tint(Color(red: 0.42, green: 0.33, blue: 0.88))
        .alert("MeshRMM", isPresented: Binding(
            get: { model.errorMessage != nil },
            set: { if !$0 { model.errorMessage = nil } }
        )) {
            Button("OK", role: .cancel) { model.errorMessage = nil }
        } message: {
            Text(model.errorMessage ?? "")
        }
        .fullScreenCover(item: $model.selectedDevice) { device in
            RemoteDesktopView(device: device)
                .environmentObject(model)
        }
    }
}

private struct MeshBrand: View {
    var compact = false
    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: "point.3.connected.trianglepath.dotted")
                .font(compact ? .headline : .title2)
                .foregroundStyle(.white)
                .frame(width: compact ? 34 : 44, height: compact ? 34 : 44)
                .background(Color(red: 0.42, green: 0.33, blue: 0.88), in: RoundedRectangle(cornerRadius: compact ? 10 : 13))
            Text("Mesh").font(compact ? .headline : .title2.bold()) + Text("RMM").font(compact ? .headline.bold() : .title2.bold()).foregroundStyle(Color(red: 0.42, green: 0.33, blue: 0.88))
        }
    }
}

struct WorkspaceView: View {
    @EnvironmentObject private var model: AppModel
    @FocusState private var focused: Bool

    var body: some View {
        ZStack {
            LinearGradient(colors: [Color(red: 0.04, green: 0.03, blue: 0.09), Color(red: 0.10, green: 0.07, blue: 0.20)], startPoint: .top, endPoint: .bottom)
                .ignoresSafeArea()
            VStack(spacing: 28) {
                MeshBrand()
                    .padding(.bottom, 8)
                VStack(spacing: 10) {
                    Text("Your devices, anywhere").font(.largeTitle.bold()).multilineTextAlignment(.center)
                    Text("Connect to your company workspace to see and control enrolled computers.")
                        .foregroundStyle(.secondary).multilineTextAlignment(.center)
                }
                VStack(alignment: .leading, spacing: 9) {
                    Text("COMPANY DASHBOARD").font(.caption.bold()).foregroundStyle(.secondary)
                    TextField("acme.meshrmm.com", text: $model.workspace)
                        .textInputAutocapitalization(.never).keyboardType(.URL).autocorrectionDisabled()
                        .focused($focused).submitLabel(.continue)
                        .onSubmit { Task { await model.connectWorkspace() } }
                        .padding(14).background(.thinMaterial, in: RoundedRectangle(cornerRadius: 13))
                }
                Button {
                    Task { await model.connectWorkspace() }
                } label: {
                    HStack { if model.isBusy { ProgressView() }; Text("Continue").fontWeight(.semibold); Image(systemName: "arrow.right") }
                        .frame(maxWidth: .infinity).padding(.vertical, 14)
                }
                .buttonStyle(.borderedProminent).disabled(model.isBusy || model.workspace.trimmingCharacters(in: .whitespaces).isEmpty)
            }
            .padding(30).frame(maxWidth: 520)
        }
        .preferredColorScheme(.dark)
        .onAppear { focused = model.workspace.isEmpty }
    }
}

struct SignInView: View {
    @EnvironmentObject private var model: AppModel

    var body: some View {
        NavigationStack {
            VStack(spacing: 28) {
                Spacer()
                MeshBrand()
                Image(systemName: "lock.shield.fill").font(.system(size: 48)).foregroundStyle(.purple)
                VStack(spacing: 8) {
                    Text(model.config?.companyName ?? "Company workspace").font(.title.bold())
                    Text("Sign in through your company’s secure WorkOS identity provider.").foregroundStyle(.secondary).multilineTextAlignment(.center)
                }
                Button {
                    Task { await model.signIn() }
                } label: {
                    HStack { if model.isBusy { ProgressView() }; Image(systemName: "person.badge.key.fill"); Text("Sign in") }
                        .frame(maxWidth: .infinity).padding(.vertical, 13)
                }
                .buttonStyle(.borderedProminent).disabled(model.isBusy)
                Spacer()
                Button("Use a different workspace") { model.forgetWorkspace() }.font(.footnote)
            }
            .padding(30).frame(maxWidth: 520)
        }
    }
}

struct DeviceListView: View {
    @EnvironmentObject private var model: AppModel
    @State private var search = ""

    private var filtered: [ManagedDevice] {
        guard !search.isEmpty else { return model.devices }
        return model.devices.filter { "\($0.name) \($0.id)".localizedCaseInsensitiveContains(search) }
    }

    var body: some View {
        NavigationStack {
            List {
                Section {
                    HStack(spacing: 22) {
                        metric("Devices", value: "\(model.devices.count)", icon: "desktopcomputer", color: .purple)
                        Divider()
                        metric("Online", value: "\(model.onlineCount)", icon: "wifi", color: .green)
                    }
                    .padding(.vertical, 8)
                }
                Section("Managed devices") {
                    if filtered.isEmpty {
                        ContentUnavailableView("No devices", systemImage: "desktopcomputer.trianglebadge.exclamationmark", description: Text("No devices match this search."))
                    }
                    ForEach(filtered) { device in
                        Button {
                            if device.connected { model.selectedDevice = device }
                        } label: {
                            HStack(spacing: 14) {
                                ZStack(alignment: .bottomTrailing) {
                                    RoundedRectangle(cornerRadius: 12).fill(device.connected ? Color.purple.opacity(0.16) : Color.secondary.opacity(0.12)).frame(width: 46, height: 46)
                                    Image(systemName: "desktopcomputer").foregroundStyle(device.connected ? .purple : .secondary)
                                    Circle().fill(device.connected ? .green : .gray).frame(width: 11, height: 11).overlay(Circle().stroke(.background, lineWidth: 2))
                                }
                                VStack(alignment: .leading, spacing: 4) {
                                    Text(device.name).font(.headline).foregroundStyle(.primary)
                                    Text(device.id).font(.caption.monospaced()).foregroundStyle(.secondary).lineLimit(1)
                                }
                                Spacer()
                                if device.connected {
                                    Image(systemName: "arrow.up.right.square.fill").foregroundStyle(.purple)
                                } else {
                                    Text("Offline").font(.caption.weight(.medium)).foregroundStyle(.secondary)
                                }
                            }
                            .contentShape(Rectangle())
                        }
                        .buttonStyle(.plain).disabled(!device.connected)
                    }
                }
            }
            .searchable(text: $search, prompt: "Search devices")
            .navigationTitle(model.config?.companyName ?? "Devices")
            .toolbar {
                ToolbarItem(placement: .topBarLeading) { MeshBrand(compact: true) }
                ToolbarItemGroup(placement: .topBarTrailing) {
                    Button { Task { try? await model.refresh() } } label: { Image(systemName: "arrow.clockwise") }
                    Menu {
                        Button("Change workspace", role: .destructive) { model.forgetWorkspace() }
                        Button("Sign out", role: .destructive) { model.signOut() }
                    } label: { Image(systemName: "ellipsis.circle") }
                }
            }
            .refreshable { try? await model.refresh() }
        }
    }

    private func metric(_ label: String, value: String, icon: String, color: Color) -> some View {
        HStack(spacing: 10) {
            Image(systemName: icon).foregroundStyle(color).frame(width: 30, height: 30).background(color.opacity(0.12), in: RoundedRectangle(cornerRadius: 8))
            VStack(alignment: .leading) { Text(value).font(.title2.bold()); Text(label).font(.caption).foregroundStyle(.secondary) }
        }.frame(maxWidth: .infinity, alignment: .leading)
    }
}
