import { useCallback, useMemo, useSyncExternalStore } from "react";
import type { MetricsReading } from "../metrics/model";
import type { DeviceCounts, DeviceLookup, DeviceStore, Topic } from "./device-store";
import type { Agent, Company, Tag } from "./types";

// Each hook re-renders its component only when what it reads changed; the
// store returns the same value until then.

function useTopic<T>(store: DeviceStore, topic: Topic, read: (store: DeviceStore) => T): T {
  const subscribe = useCallback((listener: () => void) => store.subscribe(topic, listener), [store, topic]);
  const snapshot = () => read(store);
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

// Every device, in the order the list shows them.
export const useDevices = (store: DeviceStore): readonly Agent[] => useTopic(store, "agents", (devices) => devices.agents());

// How many devices there are, and how many are online.
export const useDeviceCounts = (store: DeviceStore): DeviceCounts => useTopic(store, "agents", (devices) => devices.counts());

export const useCompanies = (store: DeviceStore): readonly Company[] => useTopic(store, "companies", (devices) => devices.companies());

export const useTags = (store: DeviceStore): readonly Tag[] => useTopic(store, "tags", (devices) => devices.tags());

// Whether the devices have loaded since sign-in.
export const useDevicesLoaded = (store: DeviceStore) => useTopic(store, "meta", (devices) => devices.loaded());

// When the devices last changed.
export const useDevicesUpdated = (store: DeviceStore) => useTopic(store, "meta", (devices) => devices.lastUpdated());

// One device, or undefined while it isn't known.
export function useDevice(store: DeviceStore, id: string): Agent | undefined {
  const subscribe = useCallback((listener: () => void) => store.subscribeDevice(id, listener), [store, id]);
  const snapshot = () => store.get(id);
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

// One online device's latest resource usage.
export function useDeviceReading(store: DeviceStore, id: string): MetricsReading | undefined {
  const subscribe = useCallback((listener: () => void) => store.subscribeReading(id, listener), [store, id]);
  const snapshot = () => store.reading(id);
  return useSyncExternalStore(subscribe, snapshot, snapshot);
}

// The devices by ID, for pages that look up those they show; a new lookup
// once any device changed.
export const useDeviceLookup = (store: DeviceStore): DeviceLookup => useTopic(store, "agents", (devices) => devices.lookup());

// Devices' names by ID, for pages that name the devices they show.
export function useDeviceNames(store: DeviceStore) {
  const lookup = useDeviceLookup(store);
  return useMemo(() => ({ get: (id: string) => lookup.get(id)?.name }), [lookup]);
}
