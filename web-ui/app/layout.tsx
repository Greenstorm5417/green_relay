import type { Metadata } from "next";
import "./globals.css";

export const metadata: Metadata = {
  title: "SMS Microservice Admin",
  description: "Administration panel for the SMS microservice.",
};

export default function RootLayout({
  children,
}: Readonly<{
  children: React.ReactNode;
}>) {
  return (
    <html lang="en" className="h-full antialiased">
      <body className="min-h-full bg-gray-50 text-gray-900">{children}</body>
    </html>
  );
}
