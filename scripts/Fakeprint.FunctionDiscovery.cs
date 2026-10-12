// Function Discovery COM helpers for fakeprint (loaded by Fakeprint.psm1 via Add-Type).
// Queries Provider\Microsoft.Networking.WSD and Layered\Microsoft.Networking.Devices.
using System;
using System.Collections.Generic;
using System.Runtime.InteropServices;
using System.Threading;

namespace Fakeprint.Fd
{
    [StructLayout(LayoutKind.Sequential)]
    public struct PROPERTYKEY
    {
        public Guid fmtid;
        public uint pid;
        public PROPERTYKEY(Guid f, uint p) { fmtid = f; pid = p; }
    }

    [StructLayout(LayoutKind.Sequential)]
    public struct PROPVARIANT
    {
        public ushort vt;
        public ushort wReserved1;
        public ushort wReserved2;
        public ushort wReserved3;
        public IntPtr p;
        public IntPtr p2;
    }

    [ComImport, Guid("886d8eeb-8cf2-4446-8d02-cdba1dbdcf99"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IPropertyStore
    {
        void GetCount(out uint cProps);
        void GetAt(uint iProp, out PROPERTYKEY pkey);
        void GetValue(ref PROPERTYKEY key, out PROPVARIANT pv);
        void SetValue(ref PROPERTYKEY key, ref PROPVARIANT pv);
        void Commit();
    }

    [ComImport, Guid("33591c10-0bed-4f02-b0ab-1530d5533ee9"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IFunctionInstance
    {
        [PreserveSig] int QueryService(ref Guid guidService, ref Guid riid, out IntPtr ppvObject);
        void GetID([MarshalAs(UnmanagedType.LPWStr)] out string ppszCoMemIdentity);
        void GetProviderInstanceID([MarshalAs(UnmanagedType.LPWStr)] out string ppszCoMemProviderInstanceIdentity);
        void OpenPropertyStore(uint dwStgAccess, out IPropertyStore ppIPropertyStore);
        void GetCategory([MarshalAs(UnmanagedType.LPWStr)] out string ppszCoMemCategory,
                         [MarshalAs(UnmanagedType.LPWStr)] out string ppszCoMemSubCategory);
    }

    [ComImport, Guid("f0a3d895-855c-42a2-948d-2f97d450ecb1"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IFunctionInstanceCollection
    {
        void GetCount(out uint pdwCount);
        void Item(uint dwIndex, out IFunctionInstance ppIFunctionInstance);
    }

    [ComImport, Guid("57cc6fd2-c09a-4289-bb72-25f04142058e"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IFunctionInstanceCollectionQuery
    {
        void AddQueryConstraint([MarshalAs(UnmanagedType.LPWStr)] string pszConstraintName,
                                [MarshalAs(UnmanagedType.LPWStr)] string pszConstraintValue);
        void AddPropertyConstraint(ref PROPERTYKEY Key, ref PROPVARIANT pv, int enumPropertyConstraint);
        [PreserveSig] int Execute(out IFunctionInstanceCollection ppIFunctionInstanceCollection);
    }

    [ComImport, Guid("5f6c1ba8-5330-422e-a368-572b244d3f87"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IFunctionDiscoveryNotification
    {
        [PreserveSig] int OnUpdate(int enumQueryUpdateAction, ulong fdqcQueryContext, IFunctionInstance pIFunctionInstance);
        [PreserveSig] int OnError(int hr, ulong fdqcQueryContext, [MarshalAs(UnmanagedType.LPWStr)] string pszProvider);
        [PreserveSig] int OnEvent(uint dwEventID, ulong fdqcQueryContext, [MarshalAs(UnmanagedType.LPWStr)] string pszProvider);
    }

    [ComImport, Guid("4df99b70-e148-4432-b004-4c9eeb535a5e"), InterfaceType(ComInterfaceType.InterfaceIsIUnknown)]
    public interface IFunctionDiscovery
    {
        void GetInstanceCollection(
            [MarshalAs(UnmanagedType.LPWStr)] string pszCategory,
            [MarshalAs(UnmanagedType.LPWStr)] string pszSubCategory,
            int fIncludeAllSubCategories,
            out IFunctionInstanceCollection ppIFunctionInstanceCollection);
        void GetInstance([MarshalAs(UnmanagedType.LPWStr)] string id, out IFunctionInstance pp);
        void CreateInstanceCollectionQuery(
            [MarshalAs(UnmanagedType.LPWStr)] string pszCategory,
            [MarshalAs(UnmanagedType.LPWStr)] string pszSubCategory,
            int fIncludeAllSubCategories,
            IFunctionDiscoveryNotification pIFunctionDiscoveryNotification,
            ref ulong pfdqcQueryContext,
            out IFunctionInstanceCollectionQuery ppIFunctionInstanceCollectionQuery);
    }

    [ComVisible(true)]
    [ClassInterface(ClassInterfaceType.None)]
    public class FdNotify : IFunctionDiscoveryNotification
    {
        public readonly List<FdHit> Hits = new List<FdHit>();
        public readonly ManualResetEvent SearchComplete = new ManualResetEvent(false);
        public string LastError;
        public string CategoryTag;

        public int OnUpdate(int action, ulong ctx, IFunctionInstance inst)
        {
            if (inst == null) return 0;
            try { Hits.Add(FdQuery.FromInstance(inst, CategoryTag, "OnUpdate:" + action)); }
            catch (Exception ex) { LastError = "OnUpdate: " + ex.Message; }
            return 0;
        }

        public int OnError(int hr, ulong ctx, string provider)
        {
            LastError = string.Format("OnError hr=0x{0:X8} provider={1}", hr, provider);
            return 0;
        }

        public int OnEvent(uint eventId, ulong ctx, string provider)
        {
            // FD_EVENTID_SEARCHCOMPLETE = 1000
            if (eventId == 1000) SearchComplete.Set();
            return 0;
        }
    }

    public class FdHit
    {
        public string Category;
        public string Source;
        public string Id;
        public string ProviderInstanceId;
        public string FriendlyName;
        public string Manufacturer;
        public string ModelName;
        public string GlobalIdentity;
        public string Types;
        public string XAddrs;
        public string ServiceAddress;
        public string IpAddress;
        public string DeviceCategory;
        public string Installable;
        public int ExecuteHr;
    }

    public class FdQueryResult
    {
        public int ExecuteHr;
        public string LastError;
        public FdHit[] Hits;
        public string Category;
    }

    public static class FdQuery
    {
        static readonly Guid PnpX = new Guid("656A3BB3-ECC0-43FD-8477-4AE0404A96CD");
        static readonly Guid InstallableFmt = new Guid("4FC5077E-B686-44BE-93E3-86CAFE368CCD");
        static readonly Guid NameFmt = new Guid("b725f130-47ef-101a-a5f1-02608c9eebac");

        [DllImport("ole32.dll")]
        static extern int PropVariantClear(ref PROPVARIANT pvar);

        [DllImport("user32.dll")]
        static extern int MsgWaitForMultipleObjects(int nCount, IntPtr pHandles, bool bWaitAll, int dwMilliseconds, int dwWakeMask);

        [DllImport("user32.dll")]
        static extern bool PeekMessage(out MSG lpMsg, IntPtr hWnd, uint wMsgFilterMin, uint wMsgFilterMax, uint wRemoveMsg);

        [DllImport("user32.dll")]
        static extern bool TranslateMessage(ref MSG lpMsg);

        [DllImport("user32.dll")]
        static extern IntPtr DispatchMessage(ref MSG lpMsg);

        [StructLayout(LayoutKind.Sequential)]
        struct MSG
        {
            public IntPtr hwnd;
            public uint message;
            public IntPtr wParam;
            public IntPtr lParam;
            public uint time;
            public int pt_x;
            public int pt_y;
        }

        static string ReadStr(IPropertyStore store, PROPERTYKEY key)
        {
            PROPVARIANT pv;
            try { store.GetValue(ref key, out pv); }
            catch { return null; }
            try
            {
                if (pv.vt == 31 || pv.vt == 8)
                {
                    if (pv.p == IntPtr.Zero) return null;
                    return Marshal.PtrToStringUni(pv.p);
                }
                if (pv.vt == 11)
                {
                    short b = (short)(pv.p.ToInt64() & 0xFFFF);
                    return (b != 0) ? "true" : "false";
                }
                if (pv.vt == 4127)
                {
                    int count = pv.p.ToInt32();
                    IntPtr arr = pv.p2;
                    if (arr == IntPtr.Zero || count <= 0) return null;
                    var parts = new List<string>();
                    for (int i = 0; i < count && i < 32; i++)
                    {
                        IntPtr s = Marshal.ReadIntPtr(arr, i * IntPtr.Size);
                        if (s != IntPtr.Zero) parts.Add(Marshal.PtrToStringUni(s));
                    }
                    return string.Join(" ", parts.ToArray());
                }
                return null;
            }
            finally
            {
                PropVariantClear(ref pv);
            }
        }

        public static FdHit FromInstance(IFunctionInstance inst, string category, string source)
        {
            string id = null, provid = null, cat = null, sub = null;
            try { inst.GetID(out id); } catch { }
            try { inst.GetProviderInstanceID(out provid); } catch { }
            try { inst.GetCategory(out cat, out sub); } catch { }

            IPropertyStore store = null;
            try { inst.OpenPropertyStore(0, out store); } catch { }

            string friendly = null, mfr = null, model = null, gid = null, types = null, xaddrs = null;
            string svc = null, ip = null, devcat = null, installable = null;
            if (store != null)
            {
                friendly = ReadStr(store, new PROPERTYKEY(PnpX, 0x3000));
                mfr = ReadStr(store, new PROPERTYKEY(PnpX, 0x2000));
                model = ReadStr(store, new PROPERTYKEY(PnpX, 0x2002));
                gid = ReadStr(store, new PROPERTYKEY(PnpX, 0x1000));
                types = ReadStr(store, new PROPERTYKEY(PnpX, 0x1001));
                xaddrs = ReadStr(store, new PROPERTYKEY(PnpX, 0x1003));
                svc = ReadStr(store, new PROPERTYKEY(PnpX, 0x4000));
                ip = ReadStr(store, new PROPERTYKEY(PnpX, 0x3009));
                devcat = ReadStr(store, new PROPERTYKEY(PnpX, 0x3004));
                installable = ReadStr(store, new PROPERTYKEY(InstallableFmt, 1));
                if (string.IsNullOrEmpty(friendly))
                    friendly = ReadStr(store, new PROPERTYKEY(NameFmt, 10));
            }

            return new FdHit
            {
                Category = string.IsNullOrEmpty(category) ? cat : category,
                Source = source,
                Id = id,
                ProviderInstanceId = provid,
                FriendlyName = friendly,
                Manufacturer = mfr,
                ModelName = model,
                GlobalIdentity = gid,
                Types = types,
                XAddrs = xaddrs,
                ServiceAddress = svc,
                IpAddress = ip,
                DeviceCategory = devcat,
                Installable = installable
            };
        }

        static void MsgWait()
        {
            MsgWaitForMultipleObjects(0, IntPtr.Zero, false, 50, 0x04FF);
            MSG msg;
            while (PeekMessage(out msg, IntPtr.Zero, 0, 0, 1))
            {
                TranslateMessage(ref msg);
                DispatchMessage(ref msg);
            }
        }

        public static FdQueryResult QueryAsync(
            string category,
            string subCategory,
            string typeConstraint,
            string directedAddress,
            int timeoutMs)
        {
            if (subCategory == null) subCategory = "";
            var notify = new FdNotify { CategoryTag = category };
            var t = Type.GetTypeFromCLSID(new Guid("C72BE2EC-8E90-452c-B29A-AB8FF1C071FC"));
            var fd = (IFunctionDiscovery)Activator.CreateInstance(t);
            ulong ctx = 0;
            IFunctionInstanceCollectionQuery q;
            fd.CreateInstanceCollectionQuery(category, subCategory, 1, notify, ref ctx, out q);
            if (!string.IsNullOrEmpty(typeConstraint))
            {
                try { q.AddQueryConstraint("Type", typeConstraint); }
                catch (Exception ex) { notify.LastError = "AddQueryConstraint Type: " + ex.Message; }
            }
            if (!string.IsNullOrEmpty(directedAddress))
            {
                try { q.AddQueryConstraint("RemoteAddress", directedAddress); }
                catch (Exception ex) { notify.LastError = "AddQueryConstraint RemoteAddress: " + ex.Message; }
            }
            IFunctionInstanceCollection unused;
            int hr = q.Execute(out unused);
            var deadline = DateTime.UtcNow.AddMilliseconds(timeoutMs);
            while (DateTime.UtcNow < deadline)
            {
                if (notify.SearchComplete.WaitOne(100)) break;
                MsgWait();
            }
            foreach (var h in notify.Hits) h.ExecuteHr = hr;
            var result = new FdQueryResult
            {
                ExecuteHr = hr,
                LastError = notify.LastError,
                Hits = notify.Hits.ToArray(),
                Category = category
            };
            GC.KeepAlive(q);
            GC.KeepAlive(notify);
            return result;
        }
    }
}

