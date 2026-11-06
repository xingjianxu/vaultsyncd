# Test-only, administrator-only. Original Winlogon secret never leaves the guest.
Add-Type -AssemblyName System.Security
Add-Type -ReferencedAssemblies System.Security -TypeDefinition @'
using System;
using System.ComponentModel;
using System.IO;
using System.Runtime.InteropServices;
using System.Security.Cryptography;
using System.Text;
public static class S099LogonSecret {
    [StructLayout(LayoutKind.Sequential)] struct Attributes {
        public uint Length; public IntPtr RootDirectory, ObjectName;
        public uint Flags; public IntPtr SecurityDescriptor, SecurityQualityOfService;
    }
    [StructLayout(LayoutKind.Sequential)] struct Unicode {
        public ushort Length, MaximumLength; public IntPtr Buffer;
    }
    [DllImport("advapi32.dll")] static extern uint LsaOpenPolicy(IntPtr system, ref Attributes attributes, uint access, out IntPtr policy);
    [DllImport("advapi32.dll")] static extern uint LsaRetrievePrivateData(IntPtr policy, ref Unicode key, out IntPtr data);
    [DllImport("advapi32.dll")] static extern uint LsaStorePrivateData(IntPtr policy, ref Unicode key, IntPtr data);
    [DllImport("advapi32.dll")] static extern uint LsaNtStatusToWinError(uint status);
    [DllImport("advapi32.dll")] static extern uint LsaFreeMemory(IntPtr data);
    [DllImport("advapi32.dll")] static extern uint LsaClose(IntPtr policy);
    static void Check(uint status) { if(status != 0) throw new Win32Exception((int)LsaNtStatusToWinError(status)); }
    static byte[] Read() {
        var attrs = new Attributes { Length = (uint)Marshal.SizeOf(typeof(Attributes)) };
        IntPtr policy; Check(LsaOpenPolicy(IntPtr.Zero, ref attrs, 4, out policy));
        var key = new Unicode { Length = 30, MaximumLength = 32, Buffer = Marshal.StringToHGlobalUni("DefaultPassword") };
        IntPtr data = IntPtr.Zero;
        try {
            uint result = LsaRetrievePrivateData(policy, ref key, out data);
            if(result == 0xC0000034) return null;
            Check(result);
            var text = (Unicode)Marshal.PtrToStructure(data, typeof(Unicode));
            var bytes = new byte[text.Length]; Marshal.Copy(text.Buffer, bytes, 0, bytes.Length); return bytes;
        } finally { if(data != IntPtr.Zero) LsaFreeMemory(data); Marshal.FreeHGlobal(key.Buffer); LsaClose(policy); }
    }
    static void Store(byte[] bytes) {
        var attrs = new Attributes { Length = (uint)Marshal.SizeOf(typeof(Attributes)) };
        IntPtr policy; Check(LsaOpenPolicy(IntPtr.Zero, ref attrs, 0x20, out policy));
        var key = new Unicode { Length = 30, MaximumLength = 32, Buffer = Marshal.StringToHGlobalUni("DefaultPassword") };
        IntPtr data = IntPtr.Zero, buffer = IntPtr.Zero;
        try {
            if(bytes != null) {
                if(bytes.Length > 65532) throw new InvalidDataException("secret too long");
                buffer = Marshal.AllocHGlobal(bytes.Length + 2);
                Marshal.Copy(bytes, 0, buffer, bytes.Length); Marshal.WriteInt16(buffer, bytes.Length, 0);
                var text = new Unicode { Length = (ushort)bytes.Length, MaximumLength = (ushort)(bytes.Length + 2), Buffer = buffer };
                data = Marshal.AllocHGlobal(Marshal.SizeOf(typeof(Unicode))); Marshal.StructureToPtr(text, data, false);
            }
            Check(LsaStorePrivateData(policy, ref key, data));
        } finally {
            if(buffer != IntPtr.Zero) { for(int i=0;i<bytes.Length+2;i++) Marshal.WriteByte(buffer,i,0); Marshal.FreeHGlobal(buffer); }
            if(data != IntPtr.Zero) Marshal.FreeHGlobal(data); Marshal.FreeHGlobal(key.Buffer); LsaClose(policy);
        }
    }
    static void Clear(byte[] bytes) { if(bytes != null) Array.Clear(bytes, 0, bytes.Length); }
    static bool Same(byte[] a, byte[] b) {
        if(a == null || b == null) return a == null && b == null;
        if(a.Length != b.Length) return false;
        int difference = 0; for(int i=0;i<a.Length;i++) difference |= a[i]^b[i]; return difference == 0;
    }
    // The encrypted payload starts with a presence flag so absent differs from empty.
    static byte[] Decode(string path) { return ProtectedData.Unprotect(File.ReadAllBytes(path), null, DataProtectionScope.LocalMachine); }
    public static void Backup(string path) {
        if(File.Exists(path)) throw new IOException("original LSA backup already exists; restore before retry");
        byte[] original = Read(), payload = null, decoded = null;
        try {
            payload = new byte[1+(original == null ? 0 : original.Length)]; payload[0] = (byte)(original == null ? 0 : 1);
            if(original != null) Buffer.BlockCopy(original,0,payload,1,original.Length);
            var encrypted = ProtectedData.Protect(payload,null,DataProtectionScope.LocalMachine);
            using(var file = new FileStream(path,FileMode.CreateNew,FileAccess.Write,FileShare.None)) { file.Write(encrypted,0,encrypted.Length); file.Flush(true); }
            decoded = Decode(path); if(!Same(payload,decoded)) throw new IOException("LSA encrypted backup verification failed");
        } finally { Clear(original); Clear(payload); Clear(decoded); }
    }
    public static void SetTestPassword(string password) {
        byte[] bytes = Encoding.Unicode.GetBytes(password), read = null;
        try { Store(bytes); read = Read(); if(!Same(bytes,read)) throw new IOException("test LSA password verification failed"); }
        finally { Clear(bytes); Clear(read); }
    }
    public static void Restore(string path) {
        byte[] payload = Decode(path), original = null, read = null;
        try {
            if(payload.Length == 0 || payload[0] > 1) throw new InvalidDataException("invalid original LSA backup");
            if(payload[0] == 1) { original = new byte[payload.Length-1]; Buffer.BlockCopy(payload,1,original,0,original.Length); }
            Store(original); read = Read(); if(!Same(original,read)) throw new IOException("original LSA restore verification failed");
        } finally { Clear(payload); Clear(original); Clear(read); }
    }
}
'@
